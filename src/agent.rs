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
use crate::machine;
use crate::tmux::Tmux;

/// One step of an agent's conversation, as its transcript says it.
#[derive(Debug, Clone, PartialEq)]
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
}

/// One entry as the service keeps it. `seq` counts the entries before it in
/// the transcript, so a line read again -- a beat that did not land, or a host
/// that restarted -- has the same `seq` and is kept once.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Line {
    pub seq: u64,
    pub at: String,
    pub from: &'static str,
    pub text: String,
}

impl Entry {
    fn line(self, seq: u64, at: String) -> Line {
        let (from, text) = match self {
            Entry::Told { text } => ("instruction", text),
            Entry::Said { text } => ("agent", text),
            Entry::Ran { tool, input } => ("tool", format!("{tool} {input}")),
        };
        Line {
            seq,
            at,
            from,
            text,
        }
    }
}

/// How far into a transcript has been accounted for: the byte after the last
/// whole line read, and the `seq` of the entry that comes next.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Cursor {
    pub offset: u64,
    pub seq: u64,
}

/// The lines read from a transcript, and where the read after this one starts.
/// A batch with no lines still moves the cursor past what it read: those bytes
/// were read and found to say nothing.
#[derive(Debug)]
pub struct Batch {
    pub lines: Vec<Line>,
    pub next: Cursor,
}

/// However much has piled up, a read stops after this many bytes of the raw
/// transcript, most of which is Claude Code's own bookkeeping.
const RAW_LIMIT: u64 = 8 << 20;

/// The most lines the service takes for one agent in one beat.
const MOST_LINES: usize = 1000;

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
    /// of lines; the rest waits for the next read. None when nothing new has
    /// been written. Only whole lines are read, so a line Claude Code is still
    /// writing is left for next time.
    pub fn read(&self, id: &str, from: Cursor, budget: usize) -> Option<Batch> {
        let path = self.claude.transcript(&claude::session_id(id))?;
        let mut file = File::open(path).ok()?;
        file.seek(SeekFrom::Start(from.offset)).ok()?;
        let mut raw = BufReader::new(file).take(RAW_LIMIT);
        let (mut next, mut lines, mut size) = (from, Vec::new(), 0);
        let mut line = Vec::new();
        loop {
            line.clear();
            match raw.read_until(b'\n', &mut line) {
                Ok(n) if n > 0 && line.ends_with(b"\n") => {}
                _ => break,
            }
            // Lossy, so a line that is not UTF-8 is passed over rather than
            // holding the cursor in front of it for ever.
            let (at, entries) = claude::read(&String::from_utf8_lossy(&line));
            let at = at.filter(|at| instant(at)).unwrap_or_else(machine::now);
            let read: Vec<Line> = entries
                .into_iter()
                .zip(next.seq..)
                .map(|(entry, seq)| entry.line(seq, at.clone()))
                .collect();
            let more: usize = read
                .iter()
                .map(|l| serde_json::to_string(l).map_or(0, |s| s.len()))
                .sum();
            // The first line is always taken, so one too large for the budget
            // cannot hold up everything after it.
            if !lines.is_empty() && (size + more > budget || lines.len() + read.len() > MOST_LINES)
            {
                break;
            }
            size += more;
            next.offset += line.len() as u64;
            next.seq += read.len() as u64;
            lines.extend(read);
        }
        (next.offset > from.offset).then_some(Batch { lines, next })
    }
}

/// Whether `at` is an instant the service takes: UTC, as Claude Code writes
/// it. One it cannot take refuses the whole beat, and every beat after it.
fn instant(at: &str) -> bool {
    let shape = b"dddd-dd-ddTdd:dd:dd";
    let at = at.as_bytes();
    at.len() > shape.len()
        && shape.iter().zip(at).all(|(s, c)| match s {
            b'd' => c.is_ascii_digit(),
            s => s == c,
        })
        && match &at[shape.len()..] {
            [b'Z'] => true,
            [b'.', fraction @ .., b'Z'] => {
                !fraction.is_empty() && fraction.iter().all(u8::is_ascii_digit)
            }
            _ => false,
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

    /// Who said what, which is all most tests look at.
    type Seen = Vec<(&'static str, String)>;

    /// Reads on from `cursor` until `done` holds of everything read since the
    /// transcript began, checking that each line follows the one before it.
    fn read_until(
        agents: &Agents,
        id: &str,
        seen: &mut Seen,
        cursor: &mut Cursor,
        done: impl Fn(&Seen) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(seen) {
            assert!(Instant::now() < deadline, "gave up; read {seen:?}");
            if let Some(batch) = agents.read(id, *cursor, 1 << 16) {
                take(seen, cursor, batch);
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn take(seen: &mut Seen, cursor: &mut Cursor, batch: Batch) {
        for line in batch.lines {
            assert_eq!(line.seq, seen.len() as u64, "{line:?}");
            seen.push((line.from, line.text));
        }
        *cursor = batch.next;
        assert_eq!(cursor.seq, seen.len() as u64);
    }

    fn told(text: &str) -> (&'static str, String) {
        ("instruction", text.into())
    }

    fn said(text: &str) -> (&'static str, String) {
        ("agent", text.into())
    }

    #[test]
    fn an_agent_is_spoken_to_while_it_works_and_answered_in_the_same_run() {
        let host = host("session", FAKE_CLAUDE);
        let agents = &host.agents;
        agents.start("a1", Some("fix the build")).unwrap();
        assert_eq!(agents.ids(), ["a1"]);
        let (mut seen, mut cursor) = (vec![], Cursor::default());

        // Read while it is still working, before it has said anything final.
        read_until(agents, "a1", &mut seen, &mut cursor, |s| {
            s.contains(&said("step one"))
        });
        assert_eq!(seen, [told("fix the build"), said("step one")]);

        agents.say("a1", "use the release branch").unwrap();
        read_until(agents, "a1", &mut seen, &mut cursor, |s| {
            s.contains(&said("which branch?"))
        });
        assert_eq!(
            seen[2..],
            [told("use the release branch"), said("which branch?")],
            "the message reached it before it stopped"
        );

        // Answering continues the run: same session, same transcript.
        agents.say("a1", "main").unwrap();
        read_until(agents, "a1", &mut seen, &mut cursor, |s| {
            s.contains(&said("on main"))
        });
        assert_eq!(seen[4..], [told("main"), said("on main")]);
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
        let (mut seen, mut cursor) = (vec![], Cursor::default());
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
        let (mut seen, mut cursor) = (vec![], Cursor::default());
        read_until(agents, "a1", &mut seen, &mut cursor, |s| {
            s.contains(&said("working"))
        });

        agents.interrupt("a1", Some("try the other way")).unwrap();
        read_until(agents, "a1", &mut seen, &mut cursor, |s| {
            s.contains(&said("on try the other way"))
        });
        assert_eq!(
            seen,
            [
                said("working"),
                told("[Request interrupted by user]"),
                told("try the other way"),
                said("on try the other way"),
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
        let (mut seen, mut cursor) = (vec![], Cursor::default());
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

    fn transcript(host: &Host, id: &str) -> PathBuf {
        let dir = host.dir.join("state/projects/-x");
        fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{}.jsonl", claude::session_id(id)))
    }

    fn append(path: &PathBuf, bytes: &[u8]) {
        use std::io::Write;
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    #[test]
    fn a_read_stops_at_its_budget_and_the_next_picks_up_there() {
        let host = host("budget", FAKE_CLAUDE);
        let line = |text: &str| {
            format!(r#"{{"type":"user","message":{{"role":"user","content":"{text}"}}}}"#) + "\n"
        };
        let body = line("one") + &line("two") + &line("three") + r#"{"type":"user","mess"#;
        fs::write(transcript(&host, "a2"), &body).unwrap();
        let one = Entry::Told { text: "one".into() }.line(0, machine::now());
        let one = serde_json::to_string(&one).unwrap().len();

        let (mut seen, mut cursor) = (vec![], Cursor::default());
        let first = host.agents.read("a2", cursor, one).unwrap();
        take(&mut seen, &mut cursor, first);
        assert_eq!(seen, [told("one")]);
        let rest = host.agents.read("a2", cursor, 1 << 16).unwrap();
        take(&mut seen, &mut cursor, rest);
        assert_eq!(seen[1..], [told("two"), told("three")]);
        assert_eq!(
            cursor.offset as usize,
            body.rfind('\n').unwrap() + 1,
            "a line still being written is left for later"
        );
        assert!(host.agents.read("a2", cursor, 1 << 16).is_none());
    }

    // The shape of a real Claude Code 2.1 transcript, which opens with lines
    // of its own bookkeeping before the first turn.
    #[test]
    fn turns_written_after_lines_that_say_nothing_are_read() {
        let host = host("bookkeeping", FAKE_CLAUDE);
        let path = transcript(&host, "a3");
        let opening = concat!(
            r#"{"type":"last-prompt","lastPrompt":"Say hello."}"#,
            "\n",
            r#"{"type":"mode","mode":"normal"}"#,
            "\n",
            r#"{"type":"permission-mode","permissionMode":"bypassPermissions"}"#,
            "\n",
            r#"{"type":"file-history-snapshot","snapshot":{}}"#,
            "\n",
        );
        append(&path, opening.as_bytes());

        let mut cursor = Cursor::default();
        let nothing = host.agents.read("a3", cursor, 1 << 16).unwrap();
        assert_eq!(nothing.lines, []);
        assert_eq!(
            nothing.next,
            Cursor {
                offset: opening.len() as u64,
                seq: 0
            },
            "read to the end of the last whole line, and found nothing to say"
        );
        cursor = nothing.next;

        append(&path, concat!(
            r#"{"type":"user","timestamp":"2026-10-27T13:34:52.123Z","message":{"role":"user","content":"Say hello."}}"#, "\n",
            r#"{"type":"a-kind-not-yet-invented","timestamp":"2026-10-27T13:34:53.000Z"}"#, "\n",
        ).as_bytes());
        append(&path, b"{\"type\":\"attachment\",\"text\":\"\xff\xfe\"}\n");
        append(&path, concat!(
            r#"{"type":"assistant","timestamp":"2026-10-27T13:34:54.5Z","message":{"id":"m1","content":[{"type":"thinking","thinking":""}],"stop_reason":"end_turn"}}"#, "\n",
            r#"{"type":"assistant","timestamp":"yesterday","message":{"id":"m1","content":[{"type":"text","text":"Hello."}],"stop_reason":"end_turn"}}"#, "\n",
            r#"{"type":"assistant","timestamp":"2026-10-27T13:34:55Z","message":{"id":"m2","content":[{"type":"tool_use","name":"Bash","input":{"command":"pwd"}}]}}"#, "\n",
        ).as_bytes());

        let batch = host.agents.read("a3", cursor, 1 << 16).unwrap();
        let read: Vec<_> = batch
            .lines
            .iter()
            .map(|l| (l.seq, l.from, l.text.as_str()))
            .collect();
        assert_eq!(
            read,
            [
                (0, "instruction", "Say hello."),
                (1, "agent", "Hello."),
                (2, "tool", r#"Bash {"command":"pwd"}"#),
            ],
            "past a kind it does not know and a line that is not UTF-8"
        );
        assert_eq!(batch.lines[0].at, "2026-10-27T13:34:52.123Z");
        assert!(
            instant(&batch.lines[1].at),
            "{} is not an instant",
            batch.lines[1].at
        );
        assert_ne!(batch.lines[1].at, "yesterday");
        assert_eq!(batch.lines[2].at, "2026-10-27T13:34:55Z");
        assert_eq!(batch.next.offset, fs::metadata(&path).unwrap().len());

        // A host that restarts reads it all again from the start, and says the
        // same lines under the same seqs.
        let again = host.agents.read("a3", Cursor::default(), 1 << 16).unwrap();
        assert_eq!(again.next, batch.next);
        assert_eq!(
            again
                .lines
                .iter()
                .map(|l| (l.seq, &l.text))
                .collect::<Vec<_>>(),
            batch
                .lines
                .iter()
                .map(|l| (l.seq, &l.text))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn only_an_instant_in_utc_is_taken_as_one() {
        for at in ["2026-10-27T13:34:52.123Z", "2026-10-27T13:34:52Z"] {
            assert!(instant(at), "{at}");
        }
        for at in [
            "",
            "yesterday",
            "2026-10-27T13:34:52",
            "2026-10-27T13:34:52+01:00",
            "2026-10-27T13:34:52.Z",
            "2026-10-27 13:34:52Z",
        ] {
            assert!(!instant(at), "{at}");
        }
        assert!(instant(&machine::now()));
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
        let (mut seen, mut cursor): (Seen, _) = (vec![], Cursor::default());
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut said = false;
        let ran_third = |s: &Seen| {
            s.iter()
                .position(|(from, text)| *from == "tool" && text.contains("third"))
        };
        // Done once it has said something after running the corrected command.
        while !ran_third(&seen).is_some_and(|i| seen[i..].iter().any(|(from, _)| *from == "agent"))
        {
            assert!(Instant::now() < deadline, "gave up; read {seen:?}");
            if !said && seen.iter().any(|(from, _)| *from == "tool") {
                agents
                    .say(&id, "Correction: echo third instead of second.")
                    .unwrap();
                said = true;
            }
            if let Some(batch) = agents.read(&id, cursor, 1 << 16) {
                take(&mut seen, &mut cursor, batch);
            }
            thread::sleep(Duration::from_millis(500));
        }
        agents.tmux.kill_server();
        eprintln!("{seen:#?}");
        let correction = seen
            .iter()
            .position(|e| e == &told("Correction: echo third instead of second."))
            .expect("the correction reached it");
        assert!(ran_third(&seen).unwrap() > correction);
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
        let (mut seen, mut cursor): (Seen, _) = (vec![], Cursor::default());
        let started = Instant::now();
        let mut interrupted = false;
        let after = told("Never mind. Reply with one word: stopped.");
        let answered = |s: &Seen| {
            s.iter()
                .position(|e| e == &after)
                .is_some_and(|i| s[i..].iter().any(|(from, _)| *from == "agent"))
        };
        while !answered(&seen) {
            // Well inside the 90 seconds the command runs for, so it was cut
            // off, not waited out.
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "gave up; read {seen:?}"
            );
            if !interrupted && seen.iter().any(|(from, _)| *from == "tool") {
                thread::sleep(Duration::from_secs(3));
                agents
                    .interrupt(&id, Some("Never mind. Reply with one word: stopped."))
                    .unwrap();
                interrupted = true;
            }
            if let Some(batch) = agents.read(&id, cursor, 1 << 16) {
                take(&mut seen, &mut cursor, batch);
            }
            thread::sleep(Duration::from_millis(500));
        }
        let ids = agents.ids();
        agents.tmux.kill_server();
        eprintln!("{seen:#?}");
        assert_eq!(ids, [id], "the session outlives the interrupt");
        assert!(
            seen.iter()
                .any(|(from, text)| *from == "instruction"
                    && text.starts_with("[Request interrupted")),
            "the turn was stopped"
        );
    }
}
