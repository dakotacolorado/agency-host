//! Everything this host knows about Claude Code: how to start it, where it
//! writes its transcript, and what that transcript's lines mean. The transcript
//! is Claude Code's own file, not a published format, and changes shape between
//! versions; so nothing outside this module reads it, and the rest of the host
//! sees only `Entry`.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::{env, fs, io};

use crate::agent::Entry;

/// Longer than this, a text is cut: one tool input can be a whole file, and a
/// transcript is for following an agent, not for recovering its work.
const LONGEST: usize = 4096;

pub struct Claude {
    program: String,
    /// `CLAUDE_CONFIG_DIR`, which moves the whole of Claude Code's state --
    /// transcripts and `.claude.json` both -- when it is set.
    config_dir: Option<PathBuf>,
    home: PathBuf,
}

impl Claude {
    pub fn new(program: impl Into<String>, config_dir: Option<PathBuf>, home: PathBuf) -> Self {
        Claude {
            program: program.into(),
            config_dir,
            home,
        }
    }

    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Self {
        Claude::new(
            "claude",
            var("CLAUDE_CONFIG_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            var("HOME").unwrap_or_default().into(),
        )
    }

    fn state_dir(&self) -> PathBuf {
        self.config_dir
            .clone()
            .unwrap_or_else(|| self.home.join(".claude"))
    }

    /// Interactive, never `-p`: a one-shot agent cannot be spoken to once it
    /// has started, which is the whole point of running it here. Skipping
    /// permissions because nobody is watching for the prompt: an agent asking
    /// one would wait for ever, holding its run.
    pub fn argv(&self, session: &str, instructions: Option<&str>) -> Vec<String> {
        let mut argv = vec![
            self.program.clone(),
            "--dangerously-skip-permissions".into(),
            "--session-id".into(),
            session.into(),
        ];
        argv.extend(instructions.map(String::from));
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
    /// predate this host's, so what Claude Code needs is passed on each session.
    pub fn env(&self) -> Vec<(String, String)> {
        self.config_dir
            .iter()
            .map(|dir| ("CLAUDE_CONFIG_DIR".into(), dir.to_string_lossy().into()))
            .collect()
    }

    /// Claude Code asks before working in a folder it has not seen, and the
    /// default answer is to exit: the Enter that sends the first message
    /// carried to it would end the agent. Its host chose the folder, so the
    /// answer is recorded as yes beforehand.
    pub fn trust(&self, dir: &Path) -> Result<(), String> {
        let path = match &self.config_dir {
            Some(dir) => dir.join(".claude.json"),
            None => self.home.join(".claude.json"),
        };
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            // Never run here, so not signed in either: nothing to pre-answer.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let mut config: Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        // Claude Code keys a folder by the path it finds itself in, which has
        // its symlinks resolved.
        let dir = dir
            .canonicalize()
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        let project = config
            .as_object_mut()
            .ok_or_else(|| format!("{}: not an object", path.display()))?
            .entry("projects")
            .or_insert_with(|| Value::Object(Default::default()))
            .as_object_mut()
            .ok_or_else(|| format!("{}: projects is not an object", path.display()))?
            .entry(dir.to_string_lossy())
            .or_insert_with(|| Value::Object(Default::default()));
        if !project.is_object() {
            return Err(format!(
                "{}: {} is not an object",
                path.display(),
                dir.display()
            ));
        }
        if project["hasTrustDialogAccepted"] == Value::Bool(true) {
            return Ok(());
        }
        project["hasTrustDialogAccepted"] = Value::Bool(true);
        // Written aside and renamed over, so a machine that dies mid-write
        // leaves the old file rather than half of a new one.
        let aside = path.with_extension("json.agent-host");
        let write = || -> io::Result<()> {
            fs::write(&aside, serde_json::to_vec_pretty(&config)?)?;
            fs::set_permissions(&aside, fs::metadata(&path)?.permissions())?;
            fs::rename(&aside, &path)
        };
        write().map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Where the session's transcript is, once Claude Code has started writing
    /// it. Looked for by its name rather than built from the folder, since how
    /// Claude Code turns a folder into a directory name is its own business.
    pub fn transcript(&self, session: &str) -> Option<PathBuf> {
        let name = format!("{session}.jsonl");
        fs::read_dir(self.state_dir().join("projects"))
            .ok()?
            .flatten()
            .map(|project| project.path().join(&name))
            .find(|path| path.is_file())
    }
}

/// Claude Code wants a uuid for a session. An agent's id is used as it stands
/// when it is one, so the two can be matched by eye; otherwise one is derived
/// from it, the same every time, so a host that restarts finds the transcript
/// again with nothing remembered.
pub fn session_id(agent: &str) -> String {
    let is_uuid = agent.len() == 36
        && agent.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        });
    if is_uuid {
        return agent.to_ascii_lowercase();
    }
    let fnv = |basis: u64| {
        agent.bytes().fold(basis, |h, b| {
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

/// Reads transcript lines in order. One assistant message is written as a line
/// per block, each carrying the message's stop reason, so the reader remembers
/// which message it last saw stop.
#[derive(Default)]
pub struct Reader {
    stopped: Option<String>,
}

impl Reader {
    pub fn line(&mut self, line: &str) -> Vec<Entry> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return vec![];
        };
        // A subagent's own conversation, and lines Claude Code adds for itself.
        if v["isSidechain"] == true || v["isMeta"] == true {
            return vec![];
        }
        let message = &v["message"];
        match v["type"].as_str() {
            Some("user") => match &message["content"] {
                Value::String(text) => vec![Entry::Told { text: cut(text) }],
                // Tool results come back as the user's turn; they are the
                // agent's own work, not something said to it.
                Value::Array(blocks) => blocks
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .map(|text| Entry::Told { text: cut(text) })
                    .collect(),
                _ => vec![],
            },
            // What was typed while the agent worked, delivered at its next step.
            Some("attachment") if v["attachment"]["type"] == "queued_command" => v["attachment"]
                ["prompt"]
                .as_str()
                .map(|text| Entry::Told { text: cut(text) })
                .into_iter()
                .collect(),
            Some("assistant") => {
                let mut entries: Vec<Entry> = message["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|b| match b["type"].as_str() {
                        Some("text") => Some(Entry::Said {
                            text: cut(b["text"].as_str()?),
                        }),
                        Some("tool_use") => Some(Entry::Ran {
                            tool: b["name"].as_str()?.into(),
                            input: cut(&b["input"].to_string()),
                        }),
                        _ => None,
                    })
                    .collect();
                if message["stop_reason"] == "end_turn" {
                    let id = message["id"].as_str().map(String::from);
                    if id.is_none() || id != self.stopped {
                        entries.push(Entry::Waiting);
                        self.stopped = id;
                    }
                }
                entries
            }
            _ => vec![],
        }
    }
}

fn cut(text: &str) -> String {
    match text.char_indices().nth(LONGEST) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read(lines: &[Value]) -> Vec<Entry> {
        let mut reader = Reader::default();
        lines
            .iter()
            .flat_map(|l| reader.line(&l.to_string()))
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
    fn reads_what_the_agent_was_told_and_what_it_said_and_did() {
        let entries = read(&[
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
        let entries = read(&[json!({
            "type":"attachment",
            "attachment":{"type":"queued_command","prompt":"Correction: echo third.\nAlso say hi.","commandMode":"prompt"}
        })]);
        assert_eq!(entries, [told("Correction: echo third.\nAlso say hi.")]);
    }

    #[test]
    fn stopping_is_read_once_per_message() {
        let entries = read(&[
            json!({"type":"assistant","message":{"id":"m2","content":[{"type":"thinking","thinking":""}],"stop_reason":"end_turn"}}),
            json!({"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"Which branch?"}],"stop_reason":"end_turn"}}),
            json!({"type":"system","subtype":"turn_duration","durationMs":1}),
            json!({"type":"user","message":{"role":"user","content":"main"}}),
            json!({"type":"assistant","message":{"id":"m3","content":[{"type":"text","text":"Done."}],"stop_reason":"end_turn"}}),
        ]);
        assert_eq!(
            entries,
            [
                Entry::Waiting,
                said("Which branch?"),
                told("main"),
                said("Done."),
                Entry::Waiting
            ]
        );
    }

    #[test]
    fn leaves_out_what_was_not_said_in_this_conversation() {
        let entries = read(&[
            json!({"type":"user","isSidechain":true,"message":{"content":"a subagent's prompt"}}),
            json!({"type":"user","isMeta":true,"message":{"content":"<local-command-caveat>"}}),
            json!({"type":"attachment","attachment":{"type":"date","date":"2026-10-27"}}),
            json!({"type":"file-history-snapshot"}),
        ]);
        assert_eq!(entries, []);
        assert_eq!(Reader::default().line("not json {"), []);
    }

    #[test]
    fn a_long_text_is_cut() {
        let long = "é".repeat(LONGEST + 10);
        let Entry::Said { text } = &read(&[json!({
            "type":"assistant","message":{"id":"m","content":[{"type":"text","text":long}],"stop_reason":"tool_use"}
        })])[0] else {
            panic!()
        };
        assert_eq!(text.chars().count(), LONGEST + 1);
        assert!(text.ends_with('…'));
    }

    #[test]
    fn a_session_is_the_agents_id_or_derived_from_it() {
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
        let dir = std::env::temp_dir().join(format!("agent-host-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn trusting_a_folder_keeps_everything_else() {
        let state = scratch("trust");
        let work = state.join("work");
        fs::create_dir(&work).unwrap();
        let config = state.join(".claude.json");
        fs::write(
            &config,
            r#"{"oauthAccount":{"id":"x"},"projects":{"/elsewhere":{"a":1}}}"#,
        )
        .unwrap();
        let claude = Claude::new("claude", Some(state.clone()), "/nonexistent".into());

        claude.trust(&work).unwrap();
        claude.trust(&work).unwrap();

        let written: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(written["oauthAccount"]["id"], "x");
        assert_eq!(written["projects"]["/elsewhere"]["a"], 1);
        assert_eq!(
            written["projects"][work.to_string_lossy().as_ref()]["hasTrustDialogAccepted"],
            true
        );
    }

    #[test]
    fn with_nothing_to_trust_against_trusting_does_nothing() {
        let state = scratch("untrusted");
        let claude = Claude::new("claude", Some(state.clone()), "/nonexistent".into());
        claude.trust(&state).unwrap();
        assert!(!state.join(".claude.json").exists());
    }

    #[test]
    fn finds_a_transcript_by_its_session() {
        let state = scratch("find");
        let claude = Claude::new("claude", Some(state.clone()), "/nonexistent".into());
        assert_eq!(claude.transcript("s1"), None);
        fs::create_dir_all(state.join("projects/-some-folder")).unwrap();
        fs::write(state.join("projects/-some-folder/s1.jsonl"), "").unwrap();
        assert_eq!(
            claude.transcript("s1"),
            Some(state.join("projects/-some-folder/s1.jsonl"))
        );
    }
}
