//! Everything this host knows about Claude Code: how to start it, what it
//! says on its stdout and what to write to its stdin, and where it keeps a
//! worker's settings and transcript.
//!
//! A session is `claude -p` over stream-json: each line on stdin is a message
//! or an answer to Claude Code, each line on stdout an event from it. Its
//! shapes are the Agent SDK's, checked against Claude Code `CHECKED`; nothing
//! outside this module reads or writes them, and the rest of the host sees
//! only `Entry`, `Read` and the lines made here. A transcript Claude Code
//! writes for itself is read only for sessions an older release of this host
//! began (see `Source` in worker.rs); its lines and stdout's
//! events share their shapes, so one reader serves both.
//!
//! Every worker has a Claude Code of its own -- settings, history, projects,
//! transcripts and the rules it works by, all in `CLAUDE_CONFIG_DIR` under its
//! home -- and the host user's login. HOME stays the user's: on macOS the
//! login is in the user's login Keychain, which a process with another HOME
//! does not search (Claude Code then says "Not logged in"), and git, gh and ssh
//! find the user's identity there too. Claude Code keys its login store by
//! `CLAUDE_CONFIG_DIR` unless `CLAUDE_SECURESTORAGE_CONFIG_DIR` says where the
//! login is, so each session is given that, set to wherever the host user's
//! own Claude Code keeps it.

use serde_json::{json, Map, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::{env, fs, io};

use crate::config::Permissions;
use crate::protocol::{Window, Windows};
use crate::worker::Entry;

/// The Claude Code release the stream-json protocol here was checked
/// against, by its own tests and `real_claude`. Another may change a shape;
/// the host says so when it starts.
pub const CHECKED: &str = "2.1.318";

/// What a worker says or is told is carried whole up to this many bytes, as
/// the JSON its line goes up in. A report is one socket message, which API
/// Gateway takes up to 128 KB, and a report may hold one such line past its
/// own budget (`TRANSCRIPT_PER_REPORT`), so this leaves room for both. As
/// characters, it is far past the longest answer a person reads.
pub const LONGEST_TEXT: usize = 64 << 10;

/// A tool's input is shown on one line and can be a whole file, and a
/// transcript is for following a worker, not for recovering its work: longer
/// than this many characters, it is cut.
const LONGEST_INPUT: usize = 4096;

/// More than the note a cut text ends with can take, at its longest numbers.
const NOTE_ROOM: usize = 64;

/// Tools a worker is not given; see `Claude::prepare`.
const DENIED: [&str; 3] = ["AskUserQuestion", "EnterPlanMode", "ExitPlanMode"];

pub struct Claude {
    program: String,
    /// The host user's own `CLAUDE_CONFIG_DIR`, if they set one: where their
    /// login is. None is Claude Code's default, `~/.claude` and the Keychain
    /// item that goes with it.
    login: Option<PathBuf>,
}

/// How a session begins: a new conversation under this id, or the one under
/// this id carried on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Begin<'a> {
    Fresh(&'a str),
    Resume(&'a str),
}

impl Claude {
    pub fn new(program: impl Into<String>, login: Option<PathBuf>) -> Self {
        Claude {
            program: program.into(),
            login,
        }
    }

    /// The host user's Claude Code, as the host's environment names it. A
    /// `CLAUDE_CONFIG_DIR` inside `workers_dir` is a worker's own state, not
    /// the user's -- the host was started from inside a worker's session --
    /// and a session given it as its login store is not logged in, so it is
    /// passed over for Claude Code's default.
    pub fn from_env(var: impl Fn(&str) -> Option<String>, workers_dir: &Path) -> Self {
        let resolved = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let login = var("CLAUDE_CONFIG_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .filter(|dir| {
                let workers = resolved(dir).starts_with(resolved(workers_dir));
                if workers {
                    crate::log!(
                        "CLAUDE_CONFIG_DIR {} is a worker's own, not this user's: sessions use \
                         Claude Code's default login",
                        dir.display()
                    );
                }
                !workers
            });
        Claude::new("claude", login)
    }

    /// Headless, and spoken to for as long as it runs: messages and answers
    /// go in on stdin, and events come out on stdout, each a JSON line. Each
    /// user message is echoed back (`--replay-user-messages`) once Claude Code
    /// takes it, which is how a delivery is known to have landed. Permission
    /// requests come to the host on stdout (`--permission-prompt-tool
    /// stdio`); with `--permission-prompts host` alone, Claude Code 2.1.318
    /// denies them itself.
    ///
    /// The session's tokens are read from `credentials`, when there is one,
    /// so that none is on a command line.
    pub fn argv(&self, begin: Begin, credentials: &Path) -> Vec<String> {
        let mut argv: Vec<String> = [
            "sh",
            "-c",
            r#"c=$1; shift; if [ -f "$c" ]; then set -a; . "$c"; set +a; fi; exec "$@""#,
            "sh",
            &credentials.to_string_lossy(),
            &self.program,
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--replay-user-messages",
            "--permission-prompt-tool",
            "stdio",
        ]
        .map(String::from)
        .to_vec();
        match begin {
            Begin::Fresh(session) => argv.extend(["--session-id".into(), session.into()]),
            Begin::Resume(session) => argv.extend(["--resume".into(), session.into()]),
        }
        argv
    }

    /// A start that cannot run is caught here, where it can still be
    /// reported, rather than as a shell that exits at once.
    pub fn installed(&self) -> Result<(), String> {
        let program = Path::new(&self.program);
        let found = if self.program.contains('/') {
            program.is_file()
        } else {
            env::var_os("PATH")
                .is_some_and(|path| env::split_paths(&path).any(|dir| dir.join(program).is_file()))
        };
        found
            .then_some(())
            .ok_or_else(|| format!("{} is not installed on this host", self.program))
    }

    /// Which Claude Code this is, as `claude --version` says it.
    pub fn version(&self) -> Option<String> {
        let out = Command::new(&self.program)
            .arg("--version")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        text.split_whitespace().next().map(String::from)
    }

    /// What Claude Code needs in each session's environment: the worker's
    /// own state, and where the shared login is. An empty
    /// `CLAUDE_SECURESTORAGE_CONFIG_DIR` is Claude Code's default store, not
    /// the worker's.
    pub fn env(&self, state: &Path) -> Vec<(String, String)> {
        vec![
            ("CLAUDE_CONFIG_DIR".into(), state.to_string_lossy().into()),
            (
                "CLAUDE_SECURESTORAGE_CONFIG_DIR".into(),
                self.login
                    .as_deref()
                    .map(|dir| dir.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
        ]
    }

    /// Readies a worker's own Claude Code state in `state` to start in `dir`.
    /// What Claude Code asks a new user, or of a folder it has not seen, is
    /// answered beforehand, so that a person who takes the session over with
    /// `claude --resume` is not asked either. Answers already there are kept,
    /// so what a worker changes for itself stays changed. `rules` are the
    /// worker's standing instructions, Claude Code's user memory, which it
    /// reads at the start of every session and keeps through compaction.
    pub fn prepare(&self, state: &Path, dir: &Path, rules: &str) -> Result<(), String> {
        fs::create_dir_all(state).map_err(|e| format!("{}: {e}", state.display()))?;
        // Claude Code keys a folder by the path it finds itself in, which has
        // its symlinks resolved.
        let dir = dir
            .canonicalize()
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        merge(&state.join(".claude.json"), |config| {
            for (key, value) in [
                ("hasCompletedOnboarding", Value::Bool(true)),
                // Asked on every start once a Chrome extension is installed.
                ("hasCompletedClaudeInChromeOnboarding", Value::Bool(true)),
                ("claudeInChromeDefaultEnabled", Value::Bool(false)),
            ] {
                config.entry(key).or_insert(value);
            }
            let project = config
                .entry("projects")
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .ok_or("projects is not an object")?
                .entry(dir.to_string_lossy())
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .ok_or_else(|| format!("{} is not an object", dir.display()))?;
            project.insert("hasTrustDialogAccepted".into(), Value::Bool(true));
            Ok(())
        })?;
        merge(&state.join("settings.json"), |settings| {
            // These tools wait on a person's answer at a terminal, which a
            // headless session has none of. Denied, a tool is gone, so a
            // worker asks in words and ends its turn.
            let deny = settings
                .entry("permissions")
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .ok_or("permissions is not an object")?
                .entry("deny")
                .or_insert_with(|| Value::Array(vec![]))
                .as_array_mut()
                .ok_or("permissions.deny is not an array")?;
            for tool in DENIED {
                let tool = Value::from(tool);
                if !deny.contains(&tool) {
                    deny.push(tool);
                }
            }
            Ok(())
        })?;
        let memory = state.join("CLAUDE.md");
        fs::write(&memory, rules).map_err(|e| format!("{}: {e}", memory.display()))
    }
}

/// A user message for stdin. `uuid` comes back on its echo.
pub fn message(text: &str, uuid: &str) -> Value {
    json!({
        "type": "user",
        "uuid": uuid,
        "session_id": "",
        "parent_tool_use_id": null,
        "message": {"role": "user", "content": text},
    })
}

/// Asks Claude Code to stop the turn it is in, as Escape does at a terminal.
pub fn interrupt(request: &str) -> Value {
    json!({
        "type": "control_request",
        "request_id": request,
        "request": {"subtype": "interrupt"},
    })
}

/// The uuid of the message `event` echoes back, if it is an echo.
pub fn echoed(event: &Value) -> Option<&str> {
    (event["type"] == "user" && event["isReplay"] == true)
        .then(|| event["uuid"].as_str())
        .flatten()
}

/// The line the host writes in a session's events before each Claude Code it
/// runs for it. A session resumed after a restart writes on in the same
/// events, and nothing Claude Code says tells its new process from the old
/// one, whose background tasks died with it.
pub fn started(at: String) -> Value {
    json!({"type": "worker_host", "subtype": "process_started", "timestamp": at})
}

/// The answer to `event` when it is a request of the host's: a permission
/// asked for is given or refused by `permissions`, and any other request is
/// refused by name, so that Claude Code does not wait on it.
pub fn answer(event: &Value, permissions: &Permissions) -> Option<Value> {
    if event["type"] != "control_request" {
        return None;
    }
    let request_id = event["request_id"].as_str()?;
    let request = &event["request"];
    let response = match request["subtype"].as_str() {
        Some("can_use_tool") => {
            let tool = request["tool_name"].as_str().unwrap_or_default();
            let decision = if permissions.allows(tool) {
                json!({"behavior": "allow", "updatedInput": request["input"]})
            } else {
                json!({
                    "behavior": "deny",
                    "message": format!("{tool} is not allowed on this host (worker-host's \
                                        permissions)"),
                })
            };
            json!({"subtype": "success", "request_id": request_id, "response": decision})
        }
        other => json!({
            "subtype": "error",
            "request_id": request_id,
            "error": format!("worker-host does not answer {:?} requests", other.unwrap_or("")),
        }),
    };
    Some(json!({"type": "control_response", "response": response}))
}

/// `event` as the host keeps it: with the instant it came, when Claude Code
/// did not say, so that a line read again after a restart reads the same.
pub fn stamped(mut event: Value, now: impl FnOnce() -> String) -> Value {
    if let Some(object) = event.as_object_mut() {
        if !object.get("timestamp").is_some_and(Value::is_string) {
            object.insert("timestamp".into(), Value::String(now()));
        }
    }
    event
}

/// Changes the JSON object in `path`, or a new one, and writes it back aside
/// and renamed over, so a machine that dies mid-write leaves the old file
/// rather than half of a new one. Readable by this user alone: Claude Code's
/// own `.claude.json` carries the account.
fn merge(
    path: &Path,
    change: impl FnOnce(&mut Map<String, Value>) -> Result<(), String>,
) -> Result<(), String> {
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let mut value = match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| at(&e))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Value::Object(Map::new()),
        Err(e) => return Err(at(&e)),
    };
    let object = value.as_object_mut().ok_or_else(|| at(&"not an object"))?;
    let before = object.clone();
    change(object).map_err(|e| at(&e))?;
    if *object == before && path.exists() {
        return Ok(());
    }
    let aside = path.with_extension("json.worker-host");
    let write = || -> io::Result<()> {
        fs::write(&aside, serde_json::to_vec_pretty(&value)?)?;
        fs::set_permissions(&aside, fs::Permissions::from_mode(0o600))?;
        fs::rename(&aside, path)
    };
    write().map_err(|e| at(&e))
}

/// Where a session's transcript is in a worker's state, once Claude Code has
/// started writing it. Looked for by its name rather than built from the
/// folder, since how Claude Code turns a folder into a directory name is its
/// own business.
pub fn transcript(state: &Path, session: &str) -> Option<PathBuf> {
    let name = format!("{session}.jsonl");
    fs::read_dir(state.join("projects"))
        .ok()?
        .flatten()
        .map(|project| project.path().join(&name))
        .find(|path| path.is_file())
}

/// Claude Code wants a uuid for a session, and for a message. A service id
/// is used as it stands when it is one, so the two can be matched by eye --
/// and a message's echo names the command it carried; otherwise one is
/// derived from it, the same every time, so a host that restarts finds the
/// session again with nothing remembered.
pub fn uuid(id: &str) -> String {
    let is_uuid = id.len() == 36
        && id.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        });
    if is_uuid {
        return id.to_ascii_lowercase();
    }
    derived(id, 4)
}

/// The uuid of a message the host makes up itself, which carries no command:
/// derived from `id` as `uuid` does, but marked version 8 (RFC 9562's for
/// uuids of one's own making), so its echo is told from a command's, whose
/// ids the service makes as version 4.
pub fn host_uuid(id: &str) -> String {
    derived(id, 8)
}

fn host_made(uuid: &str) -> bool {
    uuid.as_bytes().get(14) == Some(&b'8')
}

fn derived(id: &str, version: u64) -> String {
    let fnv = |basis: u64| {
        id.bytes().fold(basis, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    };
    let (hi, lo) = (fnv(0xcbf2_9ce4_8422_2325), fnv(0x8422_2325_cbf2_9ce4));
    let hi = (hi & !0xf000) | (version << 12);
    let lo = (lo & !(0b11 << 62)) | (0b10 << 62);
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        hi >> 32,
        (hi >> 16) & 0xffff,
        hi & 0xffff,
        lo >> 48,
        lo & 0xffff_ffff_ffff
    )
}

/// What one line says -- an event on a session's stdout, or a line of a
/// transcript Claude Code wrote -- and when.
#[derive(Debug, Default, PartialEq)]
pub struct Read {
    pub at: Option<String>,
    pub entries: Vec<Entry>,
    /// Whether the worker is waiting after this line: true when it ends its
    /// turn (a `result`, or in a transcript `stop_reason` `end_turn`), false
    /// when it shows it at work or being told something, None when it says
    /// neither.
    pub waiting: Option<bool>,
    /// How many background tasks and subagents the session has open after
    /// this line, when it says: Claude Code lists them all each time one
    /// starts or ends, and a new process has none.
    pub tasks: Option<u32>,
    /// Whether the turn under way was begun by a message the host delivered:
    /// false at a turn's start, true once a message the host wrote comes back.
    pub told: Option<bool>,
    /// The account's usage, when the line is a `rate_limit_event` that says
    /// any of it.
    pub usage: Option<Windows>,
}

/// What a `rate_limit_event` says of the account's windows. A window whose
/// numbers are missing or out of range is left out: the service refuses a
/// whole report over one negative utilization.
fn usage(v: &Value) -> Option<Windows> {
    if v["type"] != "rate_limit_event" {
        return None;
    }
    let windows = &v["rate_limit_info"]["unifiedWindows"];
    let window = |name: &str| {
        let w = &windows[name];
        let utilization = w["utilization"].as_f64().filter(|u| *u >= 0.0)?;
        let resets_at = crate::sys::epoch(w["resetsAt"].as_f64()?)?;
        Some(Window {
            utilization,
            resets_at,
        })
    };
    let read = Windows {
        five_hour: window("five_hour"),
        seven_day: window("seven_day"),
    };
    (read.five_hour.is_some() || read.seven_day.is_some()).then_some(read)
}

/// A line of a kind not read here says nothing, and the lines after it are
/// read as usual.
pub fn read(line: &str) -> Read {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Read::default();
    };
    let at = v["timestamp"].as_str().map(String::from);
    let usage = usage(&v);
    // A subagent's own conversation (on stdout, the lines under a tool use),
    // and lines Claude Code adds for itself.
    if v["isSidechain"] == true || v["isMeta"] == true || v["parent_tool_use_id"].is_string() {
        return Read {
            at,
            usage,
            ..Read::default()
        };
    }
    let message = &v["message"];
    let tasks = match (v["type"].as_str(), v["subtype"].as_str()) {
        (Some("system"), Some("background_tasks_changed")) => v["tasks"]
            .as_array()
            .map(|t| u32::try_from(t.len()).unwrap_or(u32::MAX)),
        (Some("worker_host"), Some("process_started")) => Some(0),
        _ => None,
    };
    // Claude Code replays its own task notifications too, marked with where
    // they came from; what the host writes carries no `origin`.
    let written = echoed(&v).filter(|_| v.get("origin").is_none());
    let command = written.filter(|u| !host_made(u)).map(String::from);
    let told = if v["type"] == "system" && v["subtype"] == "init" {
        Some(false)
    } else if written.is_some() {
        Some(true)
    } else {
        None
    };
    let waiting = match v["type"].as_str() {
        // A tool's result, too, is the worker at work.
        Some("user") => Some(false),
        Some("attachment") if v["attachment"]["type"] == "queued_command" => Some(false),
        // A turn begun, on stdout, which may be with nothing said: a
        // background task's end starts one.
        Some("system") if v["subtype"] == "init" => Some(false),
        // A turn ended, on stdout, however it ended: done, failed, interrupted.
        Some("result") => Some(true),
        // One message is a line per block, and only its last says how it
        // stopped; a line that does not say leaves it as it was.
        Some("assistant") => message["stop_reason"]
            .as_str()
            .map(|reason| reason == "end_turn"),
        _ => None,
    };
    // The texts of one line share one allowance: a read takes a line whole,
    // so it is the line, not each text in it, that must fit a report.
    let room = std::cell::Cell::new(LONGEST_TEXT);
    let carried = |text: &str| {
        let text = cut(text, room.get(), json_len);
        room.set(room.get().saturating_sub(text.chars().map(json_len).sum()));
        text
    };
    let entries = match v["type"].as_str() {
        Some("user") => match &message["content"] {
            Value::String(text) => vec![Entry::Told {
                text: carried(text),
                command,
            }],
            // Tool results come back as the user's turn; they are the
            // worker's own work, not something said to it.
            Value::Array(blocks) => blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .map(|text| Entry::Told {
                    text: carried(text),
                    command: None,
                })
                .collect(),
            _ => vec![],
        },
        // What was typed while the worker worked, delivered at its next step.
        Some("attachment") if v["attachment"]["type"] == "queued_command" => v["attachment"]
            ["prompt"]
            .as_str()
            .map(|text| Entry::Told {
                text: carried(text),
                command: None,
            })
            .into_iter()
            .collect(),
        // One message is written as a line per block.
        Some("assistant") => message["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|b| match b["type"].as_str() {
                Some("text") => Some(Entry::Said {
                    text: carried(b["text"].as_str()?),
                }),
                Some("tool_use") => Some(Entry::Ran {
                    tool: b["name"].as_str()?.into(),
                    input: cut(&b["input"].to_string(), LONGEST_INPUT, |_| 1),
                }),
                _ => None,
            })
            .collect(),
        _ => vec![],
    };
    Read {
        at,
        entries,
        waiting,
        tasks,
        told,
        usage,
    }
}

/// `text` as it is when its `size` is at most `longest`; otherwise as much
/// of it as fits with a note of how much that is, so whoever reads it knows
/// it was cut and how long it was. The note is the service's shape for a cut
/// text (`TranscriptLine` in worker-service), and counts in characters.
fn cut(text: &str, longest: usize, size: fn(char) -> usize) -> String {
    if text.chars().map(size).sum::<usize>() <= longest {
        return text.to_string();
    }
    // An allowance shorter than the note keeps the note alone.
    let room = longest.saturating_sub(NOTE_ROOM);
    let (mut used, mut end, mut kept) = (0, 0, 0);
    for (at, c) in text.char_indices() {
        used += size(c);
        if used > room {
            break;
        }
        (end, kept) = (at + c.len_utf8(), kept + 1);
    }
    let total = text.chars().count();
    format!("{}… [cut: {kept} of {total} characters]", &text[..end])
}

/// The bytes a character takes in a JSON string, as serde_json writes it.
fn json_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        c if c < ' ' => 6,
        c => c.len_utf8(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read_all(lines: &[Value]) -> Vec<Entry> {
        lines
            .iter()
            .flat_map(|l| read(&l.to_string()).entries)
            .collect()
    }

    fn told(text: &str) -> Entry {
        Entry::Told {
            text: text.into(),
            command: None,
        }
    }

    fn said(text: &str) -> Entry {
        Entry::Said { text: text.into() }
    }

    // The lines below are cut down from a real Claude Code 2.1 transcript.

    #[test]
    fn reads_what_the_worker_was_told_and_what_it_said_and_did() {
        let entries = read_all(&[
            json!({"type":"user","message":{"role":"user","content":"Run the tests."}}),
            json!({"type":"assistant","message":{"id":"m1","content":[{"type":"thinking","thinking":""}],"stop_reason":"tool_use"}}),
            json!({"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"Running them."}],"stop_reason":"tool_use"}}),
            json!({"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}],"stop_reason":"tool_use"}}),
            json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}),
        ]);
        assert_eq!(
            entries,
            [
                told("Run the tests."),
                said("Running them."),
                Entry::Ran {
                    tool: "Bash".into(),
                    input: r#"{"command":"cargo test"}"#.into()
                },
            ]
        );
    }

    #[test]
    fn a_message_typed_while_it_worked_is_read_as_told() {
        let entries = read_all(&[json!({
            "type":"attachment",
            "attachment":{"type":"queued_command","prompt":"Correction: echo third.\nAlso say hi.","commandMode":"prompt"}
        })]);
        assert_eq!(entries, [told("Correction: echo third.\nAlso say hi.")]);
    }

    #[test]
    fn a_turn_is_read_after_lines_of_kinds_it_does_not_know() {
        let entries = read_all(&[
            json!({"type":"last-prompt","lastPrompt":"Say hello."}),
            json!({"type":"permission-mode","permissionMode":"bypassPermissions"}),
            json!({"type":"some-kind-not-yet-invented","x":1}),
            json!({"type":"system","subtype":"turn_duration","durationMs":1}),
            json!({"type":"user","message":{"role":"user","content":"main"}}),
            json!({"type":"api-request-blob","blob":"..."}),
            json!({"type":"assistant","message":{"id":"m3","content":[{"type":"thinking","thinking":""}],"stop_reason":"end_turn"}}),
            json!({"type":"assistant","message":{"id":"m3","content":[{"type":"text","text":"Done."}],"stop_reason":"end_turn"}}),
        ]);
        assert_eq!(entries, [told("main"), said("Done.")]);
    }

    #[test]
    fn a_line_says_when_it_was_written() {
        let line = json!({"type":"user","timestamp":"2026-10-27T13:34:52.123Z","message":{"content":"hi"}});
        let read = read(&line.to_string());
        assert_eq!(read.at.as_deref(), Some("2026-10-27T13:34:52.123Z"));
        assert_eq!(read.entries, [told("hi")]);
    }

    #[test]
    fn leaves_out_what_was_not_said_in_this_conversation() {
        let entries = read_all(&[
            json!({"type":"user","isSidechain":true,"message":{"content":"a subagent's prompt"}}),
            json!({"type":"user","isMeta":true,"message":{"content":"<local-command-caveat>"}}),
            json!({"type":"attachment","attachment":{"type":"date","date":"2026-10-27"}}),
            json!({"type":"file-history-snapshot"}),
        ]);
        assert_eq!(entries, []);
        assert_eq!(read("not json {"), Read::default());
    }

    #[test]
    fn a_turn_that_ended_leaves_the_worker_waiting_until_it_is_told_something() {
        let waiting = |line: Value| read(&line.to_string()).waiting;
        assert_eq!(
            waiting(
                json!({"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"Which branch?"}],"stop_reason":"end_turn"}})
            ),
            Some(true)
        );
        for working in [
            json!({"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","name":"Bash","input":{}}],"stop_reason":"tool_use"}}),
            json!({"type":"user","message":{"role":"user","content":"main"}}),
            json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}),
            json!({"type":"attachment","attachment":{"type":"queued_command","prompt":"main"}}),
        ] {
            assert_eq!(waiting(working.clone()), Some(false), "{working}");
        }
        for neither in [
            json!({"type":"assistant","message":{"id":"m1","content":[{"type":"thinking","thinking":""}],"stop_reason":null}}),
            json!({"type":"system","subtype":"turn_duration","durationMs":1}),
            json!({"type":"user","isMeta":true,"message":{"content":"<local-command-caveat>"}}),
            json!({"type":"file-history-snapshot"}),
        ] {
            assert_eq!(waiting(neither.clone()), None, "{neither}");
        }
    }

    #[test]
    fn a_long_reply_and_a_long_message_are_read_whole() {
        let reply = "A long answer, line after line.\n".repeat(625);
        assert_eq!(reply.chars().count(), 20_000);
        let message = "Do this, then that. ".repeat(819);
        let entries = read_all(&[
            json!({"type":"user","message":{"role":"user","content":message}}),
            json!({"type":"attachment","attachment":{"type":"queued_command","prompt":message}}),
            json!({"type":"assistant","message":{"id":"m","content":[{"type":"text","text":reply}],"stop_reason":"end_turn"}}),
        ]);
        assert_eq!(entries, [told(&message), told(&message), said(&reply)]);
    }

    /// The note a cut text ends with, and the text before it.
    fn note(text: &str) -> (&str, usize, usize) {
        let at = text.rfind("… [cut: ").expect(text);
        let (kept, total) = text[at..]
            .trim_start_matches("… [cut: ")
            .trim_end_matches(" characters]")
            .split_once(" of ")
            .unwrap();
        (&text[..at], kept.parse().unwrap(), total.parse().unwrap())
    }

    #[test]
    fn a_text_too_long_to_carry_is_cut_and_says_so_and_how_long_it_was() {
        // Two bytes each, and six as JSON: the cut is by what goes up.
        for c in ['é', '\u{1}'] {
            let long = c.to_string().repeat(LONGEST_TEXT);
            let Entry::Said { text } = &read_all(&[json!({
                "type":"assistant","message":{"id":"m","content":[{"type":"text","text":long}],"stop_reason":"end_turn"}
            })])[0] else {
                panic!()
            };
            assert!(serde_json::to_string(text).unwrap().len() <= LONGEST_TEXT);
            let (kept, count, total) = note(text);
            assert_eq!((kept.chars().count(), total), (count, LONGEST_TEXT));
            assert!(long.starts_with(kept) && count > LONGEST_TEXT / 7);
        }
    }

    #[test]
    fn the_texts_of_one_line_share_one_allowance_and_each_cut_says_so() {
        let block = "y".repeat(60_000);
        let blocks: Vec<Value> = (0..3)
            .map(|_| json!({"type":"text","text":block}))
            .collect();
        let entries =
            read_all(&[json!({"type":"user","message":{"role":"user","content":blocks}})]);
        let texts: Vec<&str> = entries
            .iter()
            .map(|e| match e {
                Entry::Told { text, .. } => text.as_str(),
                e => panic!("{e:?}"),
            })
            .collect();
        assert_eq!(texts[0], block);
        let size: usize = texts
            .iter()
            .map(|t| serde_json::to_string(t).unwrap().len())
            .sum();
        assert!(size <= LONGEST_TEXT + 2 * NOTE_ROOM, "{size}");
        for cut in &texts[1..] {
            let (kept, count, total) = note(cut);
            assert_eq!((kept.chars().count(), total), (count, 60_000));
        }
    }

    #[test]
    fn a_long_tool_input_is_cut_and_says_so_and_how_long_it_was() {
        let input = json!({"content": "x".repeat(10_000)});
        let entries = read_all(&[json!({
            "type":"assistant","message":{"id":"m","content":[{"type":"tool_use","name":"Write","input":input}]}
        })]);
        let Entry::Ran { input: cut, .. } = &entries[0] else {
            panic!()
        };
        assert!(cut.chars().count() <= LONGEST_INPUT);
        let (kept, count, total) = note(cut);
        assert_eq!(total, input.to_string().chars().count());
        assert_eq!(kept.chars().count(), count);
        assert!(input.to_string().starts_with(kept));
    }

    #[test]
    fn a_claude_session_is_the_services_id_or_derived_from_it() {
        assert_eq!(
            uuid("8550F3B6-D6FC-4F1E-A46C-3E385A545A32"),
            "8550f3b6-d6fc-4f1e-a46c-3e385a545a32"
        );
        let derived = uuid("worker-7");
        assert_eq!(derived, uuid("worker-7"));
        assert_ne!(derived, uuid("worker-8"));
        assert_eq!(uuid(&derived), derived, "{derived} is not a uuid");
        assert_eq!(&derived[14..15], "4");
        let made = host_uuid("s1/2026-11-06T00:00:00Z");
        assert_eq!(uuid(&made), made, "{made} is not a uuid");
        assert!(host_made(&made) && !host_made(&uuid("worker-7")));
    }

    #[test]
    fn the_echo_of_a_message_the_host_wrote_names_its_command_and_nothing_else_does() {
        let command = |line: Value| -> Vec<Option<String>> {
            read(&line.to_string())
                .entries
                .into_iter()
                .map(|e| match e {
                    Entry::Told { command, .. } => command,
                    _ => panic!("{e:?}"),
                })
                .collect()
        };
        let id = "00000000-0000-4000-8000-0000000000a2";
        // As Claude Code 2.1.318 echoed a message written mid-turn, once the
        // tool it was in had finished.
        assert_eq!(
            command(
                json!({"type":"user","message":{"role":"user","content":"Correction: echo third instead of second."},"session_id":"s","parent_tool_use_id":null,"uuid":id,"timestamp":"2026-11-06T05:13:50.205Z","isReplay":true})
            ),
            [Some(id.to_string())]
        );
        let made = host_uuid("s1/now");
        for none in [
            json!({"type":"user","message":{"role":"user","content":"Your last session ended."},"uuid":made,"isReplay":true}),
            json!({"isReplay":true,"message":{"content":"<task-notification>\n</task-notification>","role":"user"},"origin":{"kind":"task-notification"},"parent_tool_use_id":null,"type":"user","uuid":id}),
            json!({"type":"user","message":{"role":"user","content":"In a transcript."},"uuid":id}),
            json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]},"parent_tool_use_id":null}),
            json!({"type":"attachment","attachment":{"type":"queued_command","prompt":"Typed.","source_uuid":id}}),
        ] {
            assert_eq!(command(none.clone()), [None], "{none}");
        }
    }

    // The events below are cut down from Claude Code 2.1.318's stdout.

    #[test]
    fn a_sessions_stdout_reads_as_its_transcript_did() {
        let entries = read_all(&[
            json!({"type":"system","subtype":"init","cwd":"/w","session_id":"s","tools":["Bash"]}),
            json!({"type":"user","message":{"role":"user","content":"Run the tests."},"session_id":"s","parent_tool_use_id":null,"uuid":"u1","timestamp":"2026-11-05T07:03:23.101Z","isReplay":true}),
            json!({"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}],"stop_reason":null},"parent_tool_use_id":null}),
            json!({"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"cargo test"}}}),
            json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok","tool_use_id":"t1"}]},"parent_tool_use_id":null}),
            json!({"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"a subagent's step"}]},"parent_tool_use_id":"t9"}),
            json!({"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}),
            json!({"type":"assistant","message":{"id":"m3","content":[{"type":"text","text":"Done."}],"stop_reason":null},"parent_tool_use_id":null}),
            json!({"type":"result","subtype":"success","is_error":false,"result":"Done."}),
        ]);
        assert_eq!(
            entries,
            [
                Entry::Told {
                    text: "Run the tests.".into(),
                    command: Some("u1".into())
                },
                Entry::Ran {
                    tool: "Bash".into(),
                    input: r#"{"command":"cargo test"}"#.into()
                },
                said("Done."),
            ]
        );
    }

    #[test]
    fn a_rate_limit_event_says_each_window_it_carries_and_no_other() {
        // As a session on Claude Code 2.1.318 wrote it, stamped.
        let recorded = r#"{"rate_limit_info":{"isUsingOverage":false,"overageDisabledReason":"org_level_disabled","overageStatus":"rejected","rateLimitType":"five_hour","resetsAt":1793965200,"status":"allowed","unifiedWindows":{"five_hour":{"resetsAt":1793965200,"status":"allowed","utilization":0.48},"seven_day":{"resetsAt":1794063600,"status":"allowed","utilization":0.4}}},"session_id":"ca93f5c1-ceed-487b-9f58-a8324b143b90","timestamp":"2026-11-06T08:28:17.268Z","type":"rate_limit_event","uuid":"7841a015-4552-4d11-93c0-e6a07bfd2d72"}"#;
        let window = |utilization, resets_at: &str| {
            Some(Window {
                utilization,
                resets_at: resets_at.into(),
            })
        };
        let read = read(recorded);
        assert_eq!(read.at.as_deref(), Some("2026-11-06T08:28:17.268Z"));
        assert_eq!(read.entries, []);
        assert_eq!(
            read.usage,
            Some(Windows {
                five_hour: window(0.48, "2026-11-06T11:40:00.000Z"),
                seven_day: window(0.4, "2026-11-07T15:00:00.000Z"),
            })
        );

        let usage = |windows: Value| {
            super::read(
                &json!({"type":"rate_limit_event","rate_limit_info":{"unifiedWindows":windows}})
                    .to_string(),
            )
            .usage
        };
        assert_eq!(
            usage(json!({"five_hour":{"utilization":0,"resetsAt":1793965200}})),
            Some(Windows {
                five_hour: window(0.0, "2026-11-06T11:40:00.000Z"),
                seven_day: None,
            }),
            "a window left out is not read as 0"
        );
        assert_eq!(
            usage(json!({
                "five_hour":{"utilization":-0.1,"resetsAt":1793965200},
                "seven_day":{"utilization":0.4,"resetsAt":1794063600}
            })),
            Some(Windows {
                five_hour: None,
                seven_day: window(0.4, "2026-11-07T15:00:00.000Z"),
            }),
            "a window out of range is left out"
        );
        for none in [
            json!({}),
            json!({"five_hour":{"utilization":0.5}}),
            json!({"seven_day":{"utilization":"half","resetsAt":1794063600}}),
            json!({"seven_day":{"utilization":0.5,"resetsAt":-5}}),
        ] {
            assert_eq!(usage(none.clone()), None, "{none}");
        }
        assert_eq!(
            super::read(r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#)
                .usage,
            None
        );
    }

    #[test]
    fn a_result_ends_the_turn_and_a_turn_begun_by_itself_is_work() {
        let waiting = |line: Value| read(&line.to_string()).waiting;
        for ended in [
            json!({"type":"result","subtype":"success","is_error":false}),
            json!({"type":"result","subtype":"error_during_execution","is_error":true,"terminal_reason":"aborted_tools"}),
        ] {
            assert_eq!(waiting(ended.clone()), Some(true), "{ended}");
        }
        assert_eq!(
            waiting(json!({"type":"system","subtype":"init","session_id":"s"})),
            Some(false)
        );
        for neither in [
            json!({"type":"assistant","message":{"id":"m","content":[{"type":"text","text":"Done."}],"stop_reason":null}}),
            json!({"type":"system","subtype":"task_notification"}),
            json!({"type":"control_request","request_id":"r","request":{"subtype":"can_use_tool"}}),
        ] {
            assert_eq!(waiting(neither.clone()), None, "{neither}");
        }
    }

    #[test]
    fn background_tasks_are_counted_from_the_list_claude_code_gives_and_none_in_a_new_process() {
        let tasks = |line: Value| read(&line.to_string()).tasks;
        assert_eq!(
            tasks(
                json!({"type":"system","subtype":"background_tasks_changed","tasks":[{"description":"Sleep 60 seconds then print finish time","run_id":"0mw3v5wh5-b560cdda","task_id":"bhzuwzdd1","task_type":"local_bash"}],"session_id":"s","timestamp":"2026-11-05T18:13:06.858Z","uuid":"u"})
            ),
            Some(1)
        );
        assert_eq!(
            tasks(
                json!({"type":"system","subtype":"background_tasks_changed","tasks":[
                    {"description":"Install Rust toolchain and zig under tools","run_id":"r1","task_id":"brn7g6zok","task_type":"local_bash"},
                    {"description":"base-python and template repos","run_id":"r2","subagent_type":"general-purpose","task_id":"a0bfc7e8e2fca49db","task_type":"local_agent"}
                ],"session_id":"s"})
            ),
            Some(2)
        );
        assert_eq!(
            tasks(
                json!({"type":"system","subtype":"background_tasks_changed","tasks":[],"session_id":"s"})
            ),
            Some(0)
        );
        assert_eq!(tasks(started("2026-11-05T18:13:01.000Z".into())), Some(0));
        for neither in [
            // A foreground Bash starts and ends a task too.
            json!({"type":"system","subtype":"task_started","task_id":"btn9l66xr","task_type":"local_bash","is_backgrounded":false,"session_id":"s"}),
            json!({"type":"system","subtype":"task_started","task_id":"bhzuwzdd1","task_type":"local_bash","is_backgrounded":true,"session_id":"s"}),
            json!({"type":"system","subtype":"task_updated","task_id":"bhzuwzdd1","patch":{"status":"completed","end_time":1_793_902_446_874_u64}}),
            json!({"type":"system","subtype":"task_notification","task_id":"bhzuwzdd1","status":"completed","summary":"Background command \"Sleep\" completed (exit code 0)"}),
            json!({"type":"system","subtype":"init","session_id":"s"}),
            json!({"type":"result","subtype":"success","is_error":false}),
        ] {
            assert_eq!(tasks(neither.clone()), None, "{neither}");
        }
    }

    #[test]
    fn a_turn_is_told_by_the_echo_of_a_message_not_by_a_task_notification() {
        let told = |line: Value| read(&line.to_string()).told;
        assert_eq!(
            told(json!({"type":"system","subtype":"init","session_id":"s"})),
            Some(false)
        );
        assert_eq!(
            told(
                json!({"isReplay":true,"message":{"content":"I agree with your recomendation here.","role":"user"},"parent_tool_use_id":null,"session_id":"s","timestamp":"2026-11-05T18:35:13.186Z","type":"user","uuid":"u1"})
            ),
            Some(true)
        );
        for neither in [
            // Claude Code replays its own notice of a background task's end.
            json!({"isReplay":true,"message":{"content":"<task-notification>\n<task-id>bgbp3zo73</task-id>\n<status>completed</status>\n</task-notification>","role":"user"},"origin":{"kind":"task-notification","producer":"session-task","runId":"0mw3vyy18-5c7b9c18"},"parent_tool_use_id":null,"session_id":"s","type":"user","uuid":"u2"}),
            json!({"type":"system","subtype":"task_notification","task_id":"bhzuwzdd1","status":"completed"}),
            json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok","tool_use_id":"t1"}]},"parent_tool_use_id":null}),
            json!({"type":"result","subtype":"success","is_error":false}),
        ] {
            assert_eq!(told(neither.clone()), None, "{neither}");
        }
    }

    #[test]
    fn a_message_goes_in_with_its_uuid_and_its_echo_names_it() {
        let sent = message("Say hi.", "aaaaaaaa-0000-4000-8000-000000000001");
        assert_eq!(sent["type"], "user");
        assert_eq!(sent["message"], json!({"role":"user","content":"Say hi."}));
        let echo =
            json!({"type":"user","message":sent["message"],"uuid":sent["uuid"],"isReplay":true});
        assert_eq!(echoed(&echo), Some("aaaaaaaa-0000-4000-8000-000000000001"));
        assert_eq!(
            echoed(&json!({"type":"user","uuid":"x","message":{}})),
            None
        );
        assert_eq!(interrupt("i1")["request"]["subtype"], "interrupt");
    }

    #[test]
    fn a_permission_is_answered_by_the_hosts_rule_and_any_other_request_refused() {
        let ask = |tool: &str| json!({"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":tool,"input":{"command":"ls"}}});
        let permissions = Permissions {
            allow: vec!["*".into()],
            deny: vec!["WebFetch".into()],
        };
        let allowed = answer(&ask("Bash"), &permissions).unwrap();
        assert_eq!(
            allowed,
            json!({"type":"control_response","response":{"subtype":"success","request_id":"r1",
                "response":{"behavior":"allow","updatedInput":{"command":"ls"}}}})
        );
        let denied = answer(&ask("WebFetch"), &permissions).unwrap();
        assert_eq!(denied["response"]["response"]["behavior"], "deny");
        assert!(denied["response"]["response"]["message"]
            .as_str()
            .unwrap()
            .contains("WebFetch is not allowed"));
        let other = json!({"type":"control_request","request_id":"r2","request":{"subtype":"hook_callback"}});
        let refused = answer(&other, &permissions).unwrap();
        assert_eq!(refused["response"]["subtype"], "error");
        assert_eq!(refused["response"]["request_id"], "r2");
        assert_eq!(answer(&json!({"type":"result"}), &permissions), None);
    }

    #[test]
    fn an_event_is_kept_with_when_it_came_unless_it_says() {
        let now = || "2026-11-05T00:00:00.000Z".to_string();
        assert_eq!(
            stamped(json!({"type":"result"}), now)["timestamp"],
            "2026-11-05T00:00:00.000Z"
        );
        let said = json!({"type":"user","timestamp":"2026-11-04T01:02:03.004Z"});
        assert_eq!(stamped(said.clone(), now), said);
    }

    fn scratch(name: &str) -> PathBuf {
        // Apart from the worker tests' folders, which run beside these and share names.
        let dir =
            std::env::temp_dir().join(format!("worker-host-claude-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn a_new_worker_state_answers_every_question_before_it_is_asked() {
        let root = scratch("prepare");
        let (state, work) = (root.join("home/.claude"), root.join("work"));
        fs::create_dir(&work).unwrap();
        let claude = Claude::new("claude", None);
        claude.prepare(&state, &work, "the rules").unwrap();

        let config: Value =
            serde_json::from_str(&fs::read_to_string(state.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(config["hasCompletedOnboarding"], true);
        assert_eq!(config["hasCompletedClaudeInChromeOnboarding"], true);
        assert_eq!(
            config["projects"][work.to_string_lossy().as_ref()]["hasTrustDialogAccepted"],
            true
        );
        let settings: Value =
            serde_json::from_str(&fs::read_to_string(state.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(settings.get("skipDangerousModePermissionPrompt"), None);
        assert_eq!(
            settings["permissions"]["deny"],
            json!(["AskUserQuestion", "EnterPlanMode", "ExitPlanMode"]),
            "a question nobody is at a terminal to answer"
        );
        assert_eq!(
            fs::read_to_string(state.join("CLAUDE.md")).unwrap(),
            "the rules"
        );
        let mode = fs::metadata(state.join(".claude.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn preparing_again_keeps_what_the_worker_changed() {
        let root = scratch("again");
        let (state, work) = (root.join(".claude"), root.join("work"));
        fs::create_dir_all(&state).unwrap();
        fs::create_dir(&work).unwrap();
        fs::write(
            state.join(".claude.json"),
            r#"{"oauthAccount":{"id":"x"},"claudeInChromeDefaultEnabled":true,"projects":{"/elsewhere":{"a":1}}}"#,
        )
        .unwrap();
        fs::write(
            state.join("settings.json"),
            r#"{"theme":"dark","permissions":{"deny":["WebFetch"]}}"#,
        )
        .unwrap();
        let claude = Claude::new("claude", None);
        claude.prepare(&state, &work, "old").unwrap();
        claude.prepare(&state, &work, "new").unwrap();

        let config: Value =
            serde_json::from_str(&fs::read_to_string(state.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(config["oauthAccount"]["id"], "x");
        assert_eq!(config["claudeInChromeDefaultEnabled"], true);
        assert_eq!(config["projects"]["/elsewhere"]["a"], 1);
        let settings: Value =
            serde_json::from_str(&fs::read_to_string(state.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(settings["theme"], "dark");
        assert_eq!(
            settings["permissions"]["deny"],
            json!([
                "WebFetch",
                "AskUserQuestion",
                "EnterPlanMode",
                "ExitPlanMode"
            ]),
            "denied once, alongside what the worker denied itself"
        );
        assert_eq!(
            fs::read_to_string(state.join("CLAUDE.md")).unwrap(),
            "new",
            "the rules are the host's, and follow its version"
        );
    }

    #[test]
    fn a_session_has_its_own_state_and_the_users_login() {
        let state = Path::new("/w/ana/home/.claude");
        let env = Claude::new("claude", None).env(state);
        assert_eq!(
            env,
            [
                (
                    "CLAUDE_CONFIG_DIR".to_string(),
                    state.to_string_lossy().into()
                ),
                ("CLAUDE_SECURESTORAGE_CONFIG_DIR".to_string(), String::new()),
            ],
            "empty is Claude Code's default store, the one the user signed in to"
        );
        let env = Claude::new("claude", Some("/u/claude".into())).env(state);
        assert_eq!(env[1].1, "/u/claude", "the user's own CLAUDE_CONFIG_DIR");
    }

    #[test]
    fn the_users_login_is_where_their_claude_config_dir_says_unless_it_is_a_workers() {
        let workers = scratch("login");
        let login = |dir: &str| {
            let dir = dir.to_string();
            Claude::from_env(
                |k| (k == "CLAUDE_CONFIG_DIR").then(|| dir.clone()),
                &workers,
            )
            .env(Path::new("/w/ana/home/.claude"))[1]
                .1
                .clone()
        };
        assert_eq!(login("/u/claude"), "/u/claude", "the user's own");
        let ana = workers.join("ana/home/.claude");
        assert_eq!(
            login(&ana.to_string_lossy()),
            "",
            "a host started from a worker's session: the default, not the worker's"
        );
        assert_eq!(login(""), "");
    }

    #[test]
    fn a_session_is_headless_over_stream_json_and_begins_new_or_carries_on() {
        let claude = Claude::new("claude", None);
        let fresh = claude.argv(Begin::Fresh("s1"), Path::new("/c"));
        assert_eq!(fresh[4..6], ["/c", "claude"]);
        assert_eq!(
            fresh[6..],
            [
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--replay-user-messages",
                "--permission-prompt-tool",
                "stdio",
                "--session-id",
                "s1"
            ]
        );
        assert!(!fresh.iter().any(|a| a.contains("dangerously")));
        let resumed = claude.argv(Begin::Resume("s1"), Path::new("/c"));
        assert_eq!(resumed[resumed.len() - 2..], ["--resume", "s1"]);
    }

    #[test]
    fn finds_a_transcript_by_its_session() {
        let state = scratch("find");
        assert_eq!(transcript(&state, "s1"), None);
        fs::create_dir_all(state.join("projects/-some-folder")).unwrap();
        fs::write(state.join("projects/-some-folder/s1.jsonl"), "").unwrap();
        assert_eq!(
            transcript(&state, "s1"),
            Some(state.join("projects/-some-folder/s1.jsonl"))
        );
    }
}
