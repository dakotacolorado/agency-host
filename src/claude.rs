//! Everything this host knows about Claude Code: how to start it, where it
//! keeps a worker's settings and transcript, and what that transcript's lines
//! mean. The transcript is Claude Code's own file, not a published format, and
//! changes shape between versions; so nothing outside this module reads it,
//! and the rest of the host sees only `Entry`.
//!
//! Every worker has a Claude Code of its own -- settings, history, projects,
//! transcripts and the rules it works by, all in `CLAUDE_CONFIG_DIR` under its
//! home -- and the host user's login. HOME stays the user's: on macOS the
//! login is in the user's login Keychain, which a process with another HOME
//! does not search (Claude Code then says "Not logged in"), and git, gh and ssh
//! find the user's identity there too. Claude Code keys its login store by
//! `CLAUDE_CONFIG_DIR` unless `CLAUDE_SECURESTORAGE_CONFIG_DIR` says where the
//! login is, so each session is given that, set to wherever the host user's
//! own Claude Code keeps it. Checked against Claude Code 2.1.312.

use serde_json::{Map, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::{env, fs, io};

use crate::worker::Entry;

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

pub struct Claude {
    program: String,
    /// The host user's own `CLAUDE_CONFIG_DIR`, if they set one: where their
    /// login is. None is Claude Code's default, `~/.claude` and the Keychain
    /// item that goes with it.
    login: Option<PathBuf>,
}

/// How a session begins: a new conversation under this id, or the last one
/// in the folder carried on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Begin<'a> {
    Fresh(&'a str),
    Continue,
}

impl Claude {
    pub fn new(program: impl Into<String>, login: Option<PathBuf>) -> Self {
        Claude {
            program: program.into(),
            login,
        }
    }

    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Self {
        Claude::new(
            "claude",
            var("CLAUDE_CONFIG_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
        )
    }

    /// Interactive, never `-p`: a one-shot session cannot be spoken to once
    /// it has started, which is the whole point of running it here. Skipping
    /// permissions because nobody is watching for the prompt: a worker asking
    /// one would wait for ever, holding its session.
    ///
    /// The opening message is read from `prompt` as the session starts, since
    /// tmux refuses a command longer than a few kilobytes and a message may be
    /// far longer. It is still one argument, never shell syntax: a quoted
    /// substitution is not read again.
    pub fn argv(&self, begin: Begin, prompt: &Path) -> Vec<String> {
        let mut argv: Vec<String> = [
            "sh",
            "-c",
            r#"p=$1; shift; exec "$@" "$(cat -- "$p")""#,
            "sh",
            &prompt.to_string_lossy(),
            &self.program,
            "--dangerously-skip-permissions",
        ]
        .map(String::from)
        .to_vec();
        match begin {
            Begin::Fresh(session) => argv.extend(["--session-id".into(), session.into()]),
            Begin::Continue => argv.push("--continue".into()),
        }
        argv
    }

    /// tmux starts a session for a program that is not there and then quietly
    /// ends it, so a start that cannot run is caught here, where it can still
    /// be reported.
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

    /// The tmux server keeps the environment it started with, which may
    /// predate this host's, so what Claude Code needs is passed on each
    /// session: the worker's own state, and where the shared login is. An
    /// empty `CLAUDE_SECURESTORAGE_CONFIG_DIR` is Claude Code's default store,
    /// not the worker's.
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

    /// Readies a worker's own Claude Code state in `state` to start in `dir`
    /// with nobody at its terminal: each question Claude Code asks of a new
    /// user, or of a folder it has not seen, is answered beforehand, since
    /// the Enter that sends the first message would answer it instead, and a
    /// question left open blocks the session. Answers already there are kept,
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
        // Asked before the first session that skips permissions.
        merge(&state.join("settings.json"), |settings| {
            settings
                .entry("skipDangerousModePermissionPrompt")
                .or_insert(Value::Bool(true));
            Ok(())
        })?;
        let memory = state.join("CLAUDE.md");
        fs::write(&memory, rules).map_err(|e| format!("{}: {e}", memory.display()))
    }
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

/// Claude Code wants a uuid for a session. The service's session id is used
/// as it stands when it is one, so the two can be matched by eye; otherwise
/// one is derived from it, the same every time, so a host that restarts finds
/// the transcript again with nothing remembered.
pub fn session_id(id: &str) -> String {
    let is_uuid = id.len() == 36
        && id.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        });
    if is_uuid {
        return id.to_ascii_lowercase();
    }
    let fnv = |basis: u64| {
        id.bytes().fold(basis, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    };
    let (hi, lo) = (fnv(0xcbf2_9ce4_8422_2325), fnv(0x8422_2325_cbf2_9ce4));
    // Shaped as a version 4 uuid, which is what a validator will expect.
    let hi = (hi & !0xf000) | 0x4000;
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

/// What one transcript line says, and when Claude Code wrote it.
#[derive(Debug, Default, PartialEq)]
pub struct Read {
    pub at: Option<String>,
    pub entries: Vec<Entry>,
    /// Whether the worker is waiting after this line: true when it ends its
    /// turn (`stop_reason` `end_turn`), false when it shows it at work or
    /// being told something, None when it says neither.
    pub waiting: Option<bool>,
}

/// A line of a kind not read here says nothing, and the lines after it are
/// read as usual.
pub fn read(line: &str) -> Read {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Read::default();
    };
    let at = v["timestamp"].as_str().map(String::from);
    // A subagent's own conversation, and lines Claude Code adds for itself.
    if v["isSidechain"] == true || v["isMeta"] == true {
        return Read {
            at,
            ..Read::default()
        };
    }
    let message = &v["message"];
    let waiting = match v["type"].as_str() {
        // A tool's result, too, is the worker at work.
        Some("user") => Some(false),
        Some("attachment") if v["attachment"]["type"] == "queued_command" => Some(false),
        // One message is a line per block, and only its last says how it
        // stopped; a line that does not say leaves it as it was.
        Some("assistant") => message["stop_reason"]
            .as_str()
            .map(|reason| reason == "end_turn"),
        _ => None,
    };
    let entries = match v["type"].as_str() {
        Some("user") => match &message["content"] {
            Value::String(text) => vec![Entry::Told {
                text: carried(text),
            }],
            // Tool results come back as the user's turn; they are the
            // worker's own work, not something said to it.
            Value::Array(blocks) => blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .map(|text| Entry::Told {
                    text: carried(text),
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
    }
}

/// What was said or told, as it is carried up: whole, unless it is longer
/// than `LONGEST_TEXT` as JSON.
pub fn carried(text: &str) -> String {
    cut(text, LONGEST_TEXT, json_len)
}

/// `text` as it is when its `size` is at most `longest`; otherwise as much
/// of it as fits with a note of how much that is, so whoever reads it knows
/// it was cut and how long it was. The note is the service's shape for a cut
/// text (`TranscriptLine` in worker-service), and counts in characters.
fn cut(text: &str, longest: usize, size: fn(char) -> usize) -> String {
    if text.chars().map(size).sum::<usize>() <= longest {
        return text.to_string();
    }
    let room = longest - NOTE_ROOM;
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
        Entry::Told { text: text.into() }
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
            session_id("8550F3B6-D6FC-4F1E-A46C-3E385A545A32"),
            "8550f3b6-d6fc-4f1e-a46c-3e385a545a32"
        );
        let derived = session_id("worker-7");
        assert_eq!(derived, session_id("worker-7"));
        assert_ne!(derived, session_id("worker-8"));
        assert_eq!(session_id(&derived), derived, "{derived} is not a uuid");
        assert_eq!(&derived[14..15], "4");
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
        assert_eq!(settings["skipDangerousModePermissionPrompt"], true);
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
        fs::write(state.join("settings.json"), r#"{"theme":"dark"}"#).unwrap();
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
        assert_eq!(settings["skipDangerousModePermissionPrompt"], true);
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
    fn a_session_begins_new_or_carries_on() {
        let claude = Claude::new("claude", None);
        let fresh = claude.argv(Begin::Fresh("s1"), Path::new("/p"));
        assert_eq!(
            fresh[5..],
            [
                "claude",
                "--dangerously-skip-permissions",
                "--session-id",
                "s1"
            ]
        );
        let resumed = claude.argv(Begin::Continue, Path::new("/p"));
        assert_eq!(
            resumed[4..],
            [
                "/p",
                "claude",
                "--dangerously-skip-permissions",
                "--continue"
            ]
        );
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
