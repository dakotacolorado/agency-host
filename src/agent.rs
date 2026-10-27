//! The agents on this host. Each is a Claude Code session running in tmux,
//! which takes input while it works and writes its transcript as it goes; the
//! host remembers nothing about them that tmux and the transcript do not hold.

use serde::Serialize;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use crate::claude::{self, Claude};
use crate::tmux::Tmux;

/// One step of an agent's conversation, as the host reports it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Entry {
    /// Its instructions, or a message carried to it.
    Told {
        text: String,
    },
    Said {
        text: String,
    },
    Ran {
        tool: String,
        input: String,
    },
    /// It has stopped and does nothing more until it is told something: it has
    /// finished, or it is asking. Nothing else announces this, and it is what
    /// a person most needs to see.
    Waiting,
}

/// The entries in an agent's transcript between two cursors. A cursor is a
/// byte offset into the transcript, so a batch sent twice -- a beat that did
/// not land, or a host that restarted -- is recognisable as the same one.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Batch {
    pub agent_id: String,
    pub from: u64,
    pub to: u64,
    pub entries: Vec<Entry>,
}

/// However much has piled up, a read stops after this many bytes of the raw
/// transcript, most of which is Claude Code's own bookkeeping.
const RAW_LIMIT: u64 = 8 << 20;

pub struct Agents {
    tmux: Tmux,
    claude: Claude,
    /// Where each agent gets a folder of its own to start in.
    home: PathBuf,
}

/// Between the key that stops a turn and the text that follows it: sent
/// together, the terminal reads the two as one escape sequence, and the text
/// lands while the turn is still stopping.
const SETTLE: Duration = Duration::from_millis(500);

impl Agents {
    pub fn new(tmux: Tmux, claude: Claude, home: PathBuf) -> Self {
        Agents { tmux, claude, home }
    }

    /// Runs the agent in a session of its own, in a folder of its own, told
    /// `instructions` if there are any and otherwise waiting to be told. A
    /// start is sent again until it is reported, so one the host has already
    /// done -- before a restart that lost the report -- is done, not repeated:
    /// its session is still running, or it has run and left a transcript.
    pub fn start(&self, id: &str, instructions: Option<&str>) -> Result<(), String> {
        let session = claude::session_id(id);
        if self.session(id).is_some() || self.claude.transcript(&session).is_some() {
            return Ok(());
        }
        self.claude.installed()?;
        let dir = self.home.join(id);
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        self.claude.trust(&dir)?;
        self.tmux.start(
            id,
            &dir,
            &self.claude.argv(&session, instructions),
            &self.claude.env(),
        )
    }

    pub fn ids(&self) -> Vec<String> {
        self.tmux.sessions().into_iter().map(|(id, _)| id).collect()
    }

    fn session(&self, id: &str) -> Option<String> {
        self.tmux
            .sessions()
            .into_iter()
            .find(|(tag, _)| tag == id)
            .map(|(_, session)| session)
    }

    fn running(&self, id: &str) -> Result<String, String> {
        self.session(id)
            .ok_or_else(|| format!("no agent {id} on this host"))
    }

    /// Types `text` into the agent's session now, whether it is working or
    /// waiting. Working, it takes the text at its next step rather than when it
    /// has finished; waiting on a question, the text is the answer and the same
    /// run goes on. There is one way in, whoever is speaking: a person, the
    /// agent above it, or an answer.
    pub fn say(&self, id: &str, text: &str) -> Result<(), String> {
        self.tmux.type_in(&self.running(id)?, text)
    }

    /// Stops the turn the agent is in, as Escape does for a person at its
    /// terminal: the session and its conversation go on, waiting, and `text`
    /// is what it is told next.
    pub fn interrupt(&self, id: &str, text: Option<&str>) -> Result<(), String> {
        let session = self.running(id)?;
        self.tmux.press(&session, "Escape")?;
        if let Some(text) = text {
            thread::sleep(SETTLE);
            self.tmux.type_in(&session, text)?;
        }
        Ok(())
    }

    /// What the agent has done since `from`, as much as fits in `budget` bytes
    /// of entries; the rest waits for the next read. None when nothing new has
    /// been written. Only whole lines are read, so a line Claude Code is still
    /// writing is left for next time.
    pub fn read(&self, id: &str, from: u64, budget: usize) -> Option<Batch> {
        let path = self.claude.transcript(&claude::session_id(id))?;
        let mut file = File::open(path).ok()?;
        file.seek(SeekFrom::Start(from)).ok()?;
        let mut lines = BufReader::new(file).take(RAW_LIMIT);
        let mut reader = claude::Reader::default();
        let (mut to, mut entries, mut size) = (from, Vec::new(), 0);
        let mut line = String::new();
        loop {
            line.clear();
            match lines.read_line(&mut line) {
                Ok(n) if n > 0 && line.ends_with('\n') => {}
                _ => break,
            }
            let read = reader.line(&line);
            let more: usize = read
                .iter()
                .map(|e| serde_json::to_string(e).map_or(0, |s| s.len()))
                .sum();
            // The first line is always taken, so one too large for the budget
            // cannot hold up everything after it.
            if size > 0 && size + more > budget {
                break;
            }
            size += more;
            to += line.len() as u64;
            entries.extend(read);
        }
        (to > from).then(|| Batch {
            agent_id: id.to_string(),
            from,
            to,
            entries,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use std::{fs, thread};

    /// Stands in for Claude Code, writing the same lines a real session does.
    /// It works one step, then takes what was typed while it worked -- its safe
    /// point -- and goes on; after that it stops and answers each line typed.
    const FAKE_CLAUDE: &str = r#"#!/bin/sh
[ "$1 $2" = "--dangerously-skip-permissions --session-id" ] || exit 64
dir="$CLAUDE_CONFIG_DIR/projects/-fake"; mkdir -p "$dir"; t="$dir/$3.jsonl"
w() { printf '%s\n' "$1" >> "$t"; }
w "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"$4\"}}"
w '{"type":"assistant","message":{"id":"m0","content":[{"type":"text","text":"step one"}],"stop_reason":"tool_use"}}'
sleep 1
IFS= read -r line
w "{\"type\":\"attachment\",\"attachment\":{\"type\":\"queued_command\",\"prompt\":\"$line\"}}"
w '{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"which branch?"}],"stop_reason":"end_turn"}}'
n=1
while IFS= read -r line; do
  n=$((n+1))
  w "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"$line\"}}"
  w "{\"type\":\"assistant\",\"message\":{\"id\":\"m$n\",\"content\":[{\"type\":\"text\",\"text\":\"on $line\"}],\"stop_reason\":\"end_turn\"}}"
done
"#;

    /// Stands in for Claude Code in a turn that runs until it is stopped: it
    /// gives up its turn only on Escape, and then answers each line typed.
    const BUSY_CLAUDE: &str = r#"#!/bin/sh
dir="$CLAUDE_CONFIG_DIR/projects/-fake"; mkdir -p "$dir"; t="$dir/$3.jsonl"
w() { printf '%s\n' "$1" >> "$t"; }
stty -icanon -echo min 1
w '{"type":"assistant","message":{"id":"m0","content":[{"type":"text","text":"working"}],"stop_reason":"tool_use"}}'
[ "$(dd bs=1 count=1 2>/dev/null)" = "$(printf '\033')" ] || exit 65
w '{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}'
stty icanon
n=0
while IFS= read -r line; do
  n=$((n+1))
  w "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"$line\"}}"
  w "{\"type\":\"assistant\",\"message\":{\"id\":\"m$n\",\"content\":[{\"type\":\"text\",\"text\":\"on $line\"}],\"stop_reason\":\"end_turn\"}}"
done
"#;

    struct Host {
        agents: Agents,
        dir: PathBuf,
    }

    impl Drop for Host {
        fn drop(&mut self) {
            self.agents.tmux.kill_server();
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn host(name: &str, program: &str) -> Host {
        let dir = std::env::temp_dir().join(format!("agent-host-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("claude");
        fs::write(&fake, program).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        Host {
            agents: Agents::new(
                Tmux::new(format!("agent-host-test-{}-{name}", std::process::id())),
                Claude::new(
                    fake.to_string_lossy(),
                    Some(dir.join("state")),
                    "/nonexistent".into(),
                ),
                dir.join("agents"),
            ),
            dir,
        }
    }

    /// Reads on from `cursor` until `done` holds of everything read so far.
    fn read_until(
        agents: &Agents,
        id: &str,
        seen: &mut Vec<Entry>,
        cursor: &mut u64,
        done: impl Fn(&[Entry]) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(seen) {
            assert!(Instant::now() < deadline, "gave up; read {seen:?}");
            if let Some(batch) = agents.read(id, *cursor, 1 << 16) {
                assert_eq!(batch.from, *cursor);
                *cursor = batch.to;
                seen.extend(batch.entries);
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn told(text: &str) -> Entry {
        Entry::Told { text: text.into() }
    }

    fn said(text: &str) -> Entry {
        Entry::Said { text: text.into() }
    }

    #[test]
    fn an_agent_is_spoken_to_while_it_works_and_answered_in_the_same_run() {
        let host = host("session", FAKE_CLAUDE);
        let agents = &host.agents;
        agents.start("a1", Some("fix the build")).unwrap();
        assert_eq!(agents.ids(), ["a1"]);
        let (mut seen, mut cursor) = (vec![], 0);

        // Read while it is still working, before it has said anything final.
        read_until(agents, "a1", &mut seen, &mut cursor, |s| {
            s.contains(&said("step one"))
        });
        assert_eq!(seen, [told("fix the build"), said("step one")]);

        agents.say("a1", "use the release branch").unwrap();
        read_until(agents, "a1", &mut seen, &mut cursor, |s| {
            s.contains(&Entry::Waiting)
        });
        assert_eq!(
            seen[2..],
            [
                told("use the release branch"),
                said("which branch?"),
                Entry::Waiting
            ],
            "the message reached it before it stopped"
        );

        // Answering continues the run: same session, same transcript.
        agents.say("a1", "main").unwrap();
        read_until(agents, "a1", &mut seen, &mut cursor, |s| {
            s.contains(&said("on main"))
        });
        assert_eq!(seen[5..], [told("main"), said("on main"), Entry::Waiting]);
        assert_eq!(agents.ids(), ["a1"]);
    }

    #[test]
    fn a_start_command_runs_the_agent_in_its_own_session_and_folder() {
        let host = host("start", FAKE_CLAUDE);
        let command: crate::beat::Command = serde_json::from_str(
            r#"{"id":"c1","kind":"start","agentId":"a1","ticketId":null,"role":"worker","level":1,"createdBy":null}"#,
        )
        .unwrap();
        let report = crate::beat::take(&command, &host.agents);
        assert!(report.ok, "{report:?}");
        assert_eq!(host.agents.ids(), ["a1"]);
        assert!(host.dir.join("agents/a1").is_dir());
        let (mut seen, mut cursor) = (vec![], 0);
        read_until(&host.agents, "a1", &mut seen, &mut cursor, |s| {
            s.contains(&said("step one"))
        });
        assert_eq!(seen[0], told(""), "started with nothing to do but wait");
    }

    #[test]
    fn an_interrupt_stops_the_turn_and_the_session_goes_on() {
        let host = host("interrupt", BUSY_CLAUDE);
        let agents = &host.agents;
        agents.start("a1", None).unwrap();
        let (mut seen, mut cursor) = (vec![], 0);
        read_until(agents, "a1", &mut seen, &mut cursor, |s| {
            s.contains(&said("working"))
        });

        agents.interrupt("a1", Some("try the other way")).unwrap();
        read_until(agents, "a1", &mut seen, &mut cursor, |s| {
            s.contains(&Entry::Waiting)
        });
        assert_eq!(
            seen,
            [
                said("working"),
                told("[Request interrupted by user]"),
                told("try the other way"),
                said("on try the other way"),
                Entry::Waiting
            ]
        );
        assert_eq!(agents.ids(), ["a1"], "the session outlives the interrupt");
    }

    #[test]
    fn a_start_already_done_is_not_done_again() {
        let host = host("again", FAKE_CLAUDE);
        host.agents.start("a1", Some("fix the build")).unwrap();
        // A host that restarts has nothing but tmux and the transcripts.
        let restarted = Agents::new(
            Tmux::new(format!("agent-host-test-{}-again", std::process::id())),
            Claude::new(
                host.dir.join("claude").to_string_lossy(),
                Some(host.dir.join("state")),
                "/nonexistent".into(),
            ),
            host.dir.join("agents"),
        );
        restarted.start("a1", Some("fix the build")).unwrap();
        assert_eq!(restarted.ids(), ["a1"]);

        // One that has run and ended left its transcript behind.
        let (mut seen, mut cursor) = (vec![], 0);
        read_until(&restarted, "a1", &mut seen, &mut cursor, |s| !s.is_empty());
        restarted.tmux.kill_server();
        restarted.start("a1", Some("fix the build")).unwrap();
        assert!(restarted.ids().is_empty(), "started a finished agent again");
    }

    #[test]
    fn a_start_that_cannot_run_says_why() {
        let host = host("missing", FAKE_CLAUDE);
        let agents = Agents::new(
            Tmux::new(format!("agent-host-test-{}-missing", std::process::id())),
            Claude::new("/nonexistent/claude", None, "/nonexistent".into()),
            host.dir.join("agents"),
        );
        let err = agents.start("a1", None).unwrap_err();
        assert!(
            err.contains("/nonexistent/claude is not installed"),
            "{err}"
        );
        assert!(agents.ids().is_empty());
    }

    #[test]
    fn an_interrupt_for_an_agent_not_here_says_so() {
        let host = host("absent-interrupt", FAKE_CLAUDE);
        let err = host.agents.interrupt("nobody", None).unwrap_err();
        assert!(err.contains("no agent nobody"), "{err}");
    }

    #[test]
    fn a_message_to_an_agent_not_here_says_so() {
        let host = host("absent", FAKE_CLAUDE);
        let err = host.agents.say("nobody", "hello").unwrap_err();
        assert!(err.contains("no agent nobody"), "{err}");
    }

    #[test]
    fn a_read_stops_at_its_budget_and_the_next_picks_up_there() {
        let host = host("budget", FAKE_CLAUDE);
        let session = claude::session_id("a2");
        let dir = host.dir.join("state/projects/-x");
        fs::create_dir_all(&dir).unwrap();
        let line = |text: &str| {
            format!(r#"{{"type":"user","message":{{"role":"user","content":"{text}"}}}}"#) + "\n"
        };
        let body = line("one") + &line("two") + &line("three") + r#"{"type":"user","mess"#;
        fs::write(dir.join(format!("{session}.jsonl")), &body).unwrap();
        let one = serde_json::to_string(&told("one")).unwrap().len();

        let first = host.agents.read("a2", 0, one).unwrap();
        assert_eq!(first.entries, [told("one")]);
        let rest = host.agents.read("a2", first.to, 1 << 16).unwrap();
        assert_eq!(rest.entries, [told("two"), told("three")]);
        assert_eq!(
            rest.to as usize,
            body.rfind('\n').unwrap() + 1,
            "a line still being written is left for later"
        );
        assert!(host.agents.read("a2", rest.to, 1 << 16).is_none());
    }

    /// Against Claude Code itself, which costs a real session:
    /// `cargo test -- --ignored real_claude`.
    #[test]
    #[ignore]
    fn real_claude() {
        let dir = std::env::temp_dir().join(format!("agent-host-real-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let tmux = Tmux::new(format!("agent-host-test-real-{}", std::process::id()));
        let agents = Agents::new(tmux, Claude::from_env(|k| std::env::var(k).ok()), dir);
        let id = format!("real-{}", std::process::id());
        agents
            .start(
                &id,
                Some(
                    "Run this shell command in the foreground: python3 -c \"import time; time.sleep(15)\". \
                 Then run: echo second. Then reply with one word: done.",
                ),
            )
            .unwrap();
        let (mut seen, mut cursor) = (vec![], 0);
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut said = false;
        while !seen.contains(&Entry::Waiting) {
            assert!(Instant::now() < deadline, "gave up; read {seen:?}");
            if !said && seen.iter().any(|e| matches!(e, Entry::Ran { .. })) {
                agents
                    .say(&id, "Correction: echo third instead of second.")
                    .unwrap();
                said = true;
            }
            if let Some(batch) = agents.read(&id, cursor, 1 << 16) {
                cursor = batch.to;
                seen.extend(batch.entries);
            }
            thread::sleep(Duration::from_millis(500));
        }
        agents.tmux.kill_server();
        eprintln!("{seen:#?}");
        let correction = seen
            .iter()
            .position(|e| e == &told("Correction: echo third instead of second."))
            .expect("the correction reached it");
        assert!(seen[correction..]
            .iter()
            .any(|e| matches!(e, Entry::Ran { input, .. } if input.contains("third"))));
    }

    /// Interrupting Claude Code itself, mid-command, as the page does:
    /// `cargo test -- --ignored real_claude`.
    #[test]
    #[ignore]
    fn real_claude_interrupted() {
        let dir = std::env::temp_dir().join(format!("agent-host-real-i-{}", std::process::id()));
        let tmux = Tmux::new(format!("agent-host-test-real-i-{}", std::process::id()));
        let agents = Agents::new(tmux, Claude::from_env(|k| std::env::var(k).ok()), dir);
        let id = format!("real-i-{}", std::process::id());
        agents.start(&id, None).unwrap();
        thread::sleep(Duration::from_secs(5));
        agents
            .say(
                &id,
                "Run this shell command in the foreground: python3 -c \"import time; time.sleep(90)\". \
                 Then reply with one word: slept.",
            )
            .unwrap();
        let (mut seen, mut cursor) = (vec![], 0);
        let started = Instant::now();
        let mut interrupted = false;
        let after = told("Never mind. Reply with one word: stopped.");
        let answered = |s: &[Entry]| {
            s.iter()
                .position(|e| e == &after)
                .is_some_and(|i| s[i..].contains(&Entry::Waiting))
        };
        while !answered(&seen) {
            // Well inside the 90 seconds the command runs for, so it was cut
            // off, not waited out.
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "gave up; read {seen:?}"
            );
            if !interrupted && seen.iter().any(|e| matches!(e, Entry::Ran { .. })) {
                thread::sleep(Duration::from_secs(3));
                agents
                    .interrupt(&id, Some("Never mind. Reply with one word: stopped."))
                    .unwrap();
                interrupted = true;
            }
            if let Some(batch) = agents.read(&id, cursor, 1 << 16) {
                cursor = batch.to;
                seen.extend(batch.entries);
            }
            thread::sleep(Duration::from_millis(500));
        }
        let ids = agents.ids();
        agents.tmux.kill_server();
        eprintln!("{seen:#?}");
        assert_eq!(ids, [id], "the session outlives the interrupt");
        assert!(
            seen.iter().any(
                |e| matches!(e, Entry::Told { text } if text.starts_with("[Request interrupted"))
            ),
            "the turn was stopped"
        );
    }
}
