//! The agents on this host. Each is a Claude Code session running in tmux,
//! which takes input while it works and writes its transcript as it goes; the
//! host remembers nothing about them that tmux and the transcript do not hold.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::claude::{self, Claude};
use crate::machine;
use crate::tmux::Tmux;
use crate::workspace;

/// Put ahead of every brief, because every agent on the fleet is started
/// through a host. Written first in agent-rig's prompts/worker.md, which still
/// has its own copy: a change to one is not a change to the other. They differ
/// on pushing, deliberately: the rig merges for the agents it runs on one
/// machine, while a host can vanish, and a pushed branch is what outlives it.
const CONDUCT: &str = include_str!("conduct.md");

/// The opening prompt is one argument to Claude Code, and linux takes no
/// single argument longer than this.
const LONGEST_PROMPT: usize = 128 << 10;

/// What a `start` says of the work, exactly as it was placed.
#[derive(Debug, Default)]
pub struct Placement<'a> {
    /// A checkout this host has, by the name its config gives it.
    pub repository: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub brief: Option<&'a str>,
    pub credential: Option<&'a Credential>,
}

/// An agent's own bearer token for the fleet's `/fleet` routes, as the service
/// issued it on its `start`. It has no `Display`, and its `Debug` names it
/// without showing it, so no log line or error message can carry it by accident.
#[derive(Deserialize)]
#[serde(transparent)]
pub struct Credential(String);

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credential(..)")
    }
}

/// Where an agent's credential is kept: in its own folder, beside its prompt
/// and outside the worktree it commits from, readable by this user alone.
const CREDENTIAL: &str = "credential";

/// What stands in a transcript line where the agent's credential was.
const REDACTED: &str = "[credential]";

/// Writes the credential once. A start comes again until it is reported, with
/// the same credential, and the first one written is the one kept.
fn keep(file: &Path, credential: &Credential) -> Result<(), String> {
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(file)
        .and_then(|mut f| f.write_all(credential.0.as_bytes()));
    match written {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => {
            Err(format!("{}: {e}", file.display()))
        }
        _ => Ok(()),
    }
}

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

/// One entry as the service keeps it. `seq` counts the entries before it in the
/// transcript, so a line read again -- a heartbeat that did not land, or a host
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
/// whole line read, the `seq` of the entry that comes next, and whether, as
/// of that line, the agent's last turn has ended with nothing said to it since.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Cursor {
    pub offset: u64,
    pub seq: u64,
    pub waiting: bool,
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

/// The most lines the service takes for one agent in one heartbeat.
const MOST_LINES: usize = 1000;

pub struct Agents {
    tmux: Tmux,
    claude: Claude,
    /// Where each agent gets a folder of its own to start in.
    home: PathBuf,
    repositories: BTreeMap<String, PathBuf>,
    /// The service this host sends heartbeats to, which an agent is told as
    /// `FLEET_URL`.
    fleet_url: String,
}

/// Between the key that stops a turn and the text that follows it: sent
/// together, the terminal reads the two as one escape sequence, and the text
/// lands while the turn is still stopping.
const SETTLE: Duration = Duration::from_millis(500);

impl Agents {
    pub fn new(
        tmux: Tmux,
        claude: Claude,
        home: PathBuf,
        repositories: BTreeMap<String, PathBuf>,
        fleet_url: impl Into<String>,
    ) -> Self {
        Agents {
            tmux,
            claude,
            home,
            repositories,
            fleet_url: fleet_url.into(),
        }
    }

    /// Runs the agent in a session of its own, in a folder of its own, told
    /// the standing rules and then its brief, or, with no brief, to wait to be
    /// told. Placed for a repository, the folder holds a worktree of it, and a
    /// worktree that cannot be made fails the start: an agent started without
    /// the code it was placed for would spend its run finding that out.
    ///
    /// A start is sent again until it is reported, so one the host has already
    /// done -- before a restart that lost the report -- is done, not repeated:
    /// its session is still running, or it has run and left a transcript.
    ///
    /// The agent's session has `FLEET_URL`, `AGENT_ID` and, when the start
    /// carried one, `FLEET_TOKEN`: its own credential. The credential is never
    /// an argument to anything, where `ps` would show it to the machine; it is
    /// written to a file only this user can read, outside the worktree, and
    /// the session's shell reads it into the environment as Claude Code starts.
    pub fn start(&self, id: &str, placement: &Placement) -> Result<(), String> {
        let session = claude::session_id(id);
        if self.session(id).is_some() || self.claude.transcript(&session).is_some() {
            return Ok(());
        }
        self.claude.installed()?;
        let home = self.home.join(id);
        fs::create_dir_all(&home).map_err(|e| format!("{}: {e}", home.display()))?;
        let credential = home.join(CREDENTIAL);
        if let Some(given) = placement.credential {
            keep(&credential, given)?;
        }
        let (dir, workspace) = match (placement.repository, placement.branch) {
            (None, None) => (
                home.clone(),
                format!(
                    "You are in `{}`, a folder of your own with no code in it: the work you \
                     were placed for named no repository.",
                    home.display()
                ),
            ),
            (None, Some(branch)) => {
                return Err(format!(
                    "branch {branch} was named with no repository to be on"
                ))
            }
            (Some(name), branch) => {
                let checkout = self.repositories.get(name).ok_or_else(|| {
                    format!(
                        "this host has no checkout of {name}: its config names none by that name"
                    )
                })?;
                let ws = workspace::prepare(id, checkout, &home.join(name), branch)?;
                let line = format!(
                    "You are in `{}`, a git worktree of {name} on branch `{}`, made for you \
                     alone: no other agent works in this tree.",
                    ws.dir.display(),
                    ws.branch
                );
                (ws.dir, line)
            }
        };
        let prompt = CONDUCT.replace("{{WORKSPACE}}", &workspace)
            + placement
                .brief
                .unwrap_or("Nothing has been asked of you yet. Wait to be told what the work is.");
        if prompt.len() > LONGEST_PROMPT {
            return Err(format!(
                "the brief and the rules ahead of it come to {} bytes, more than the {LONGEST_PROMPT} \
                 one argument can carry",
                prompt.len()
            ));
        }
        let file = home.join("prompt.md");
        fs::write(&file, &prompt).map_err(|e| format!("{}: {e}", file.display()))?;
        self.claude.trust(&dir)?;
        let mut env = self.claude.env();
        env.push(("FLEET_URL".into(), self.fleet_url.clone()));
        env.push(("AGENT_ID".into(), id.into()));
        self.tmux.start(
            id,
            &dir,
            &self
                .claude
                .argv(&session, &file, credential.is_file().then_some(&credential)),
            &env,
        )
    }

    /// The agent's credential, if it was given one, so that it can be kept
    /// out of what is read from its transcript.
    fn credential(&self, id: &str) -> Option<String> {
        fs::read_to_string(self.home.join(id).join(CREDENTIAL))
            .ok()
            .filter(|c| !c.is_empty())
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
    /// agent above it, or an answer. A message carried for the service goes
    /// through `Deliveries`, which waits until the session is listening.
    pub fn say(&self, id: &str, text: &str) -> Result<(), String> {
        self.tmux.type_in(&self.running(id)?, text)
    }

    /// Whether Claude Code has answered anything in the transcript. Until it
    /// has, it may still be starting, and what is typed into its terminal then
    /// is lost without a trace.
    fn listening(&self, id: &str) -> bool {
        self.scan(id, 0, |entries| {
            entries
                .iter()
                .any(|e| matches!(e, Entry::Said { .. } | Entry::Ran { .. }))
        })
    }

    /// Where the transcript ends now, so what is written after can be told apart.
    fn end(&self, id: &str) -> u64 {
        self.claude
            .transcript(&claude::session_id(id))
            .and_then(|path| fs::metadata(path).ok())
            .map_or(0, |m| m.len())
    }

    /// Whether the agent has been told `text` since `offset`, as typed or as
    /// Claude Code trimmed it.
    fn told_since(&self, id: &str, offset: u64, text: &str) -> bool {
        let typed = [claude::cut(text), claude::cut(text.trim())];
        self.scan(id, offset, |entries| {
            entries
                .iter()
                .any(|e| matches!(e, Entry::Told { text } if typed.contains(text)))
        })
    }

    /// Whether `found` holds of any whole line of the transcript after `offset`.
    /// A line cut through by `offset` reads as nothing.
    fn scan(&self, id: &str, offset: u64, found: impl Fn(&[Entry]) -> bool) -> bool {
        let Some(mut file) = self
            .claude
            .transcript(&claude::session_id(id))
            .and_then(|path| File::open(path).ok())
        else {
            return false;
        };
        if file.seek(SeekFrom::Start(offset)).is_err() {
            return false;
        }
        let mut raw = BufReader::new(file).take(RAW_LIMIT);
        let mut line = Vec::new();
        loop {
            line.clear();
            match raw.read_until(b'\n', &mut line) {
                Ok(n) if n > 0 && line.ends_with(b"\n") => {}
                _ => return false,
            }
            if found(&claude::read(&String::from_utf8_lossy(&line)).entries) {
                return true;
            }
        }
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

    /// Ends the agent's session, and with it the agent. Its folder, worktree
    /// and branch are left as they are, for inspection and for whatever comes
    /// after it. One not running is already stopped, which is what was asked.
    pub fn stop(&self, id: &str) -> Result<(), String> {
        match self.session(id) {
            Some(session) => self.tmux.kill_session(&session),
            None => Ok(()),
        }
    }

    /// What the agent has done since `from`, as much as fits in `budget` bytes
    /// of lines; the rest waits for the next read. None when nothing new has
    /// been written. Only whole lines are read, so a line Claude Code is still
    /// writing is left for next time.
    ///
    /// An agent may print its own credential, or put it in a command; where
    /// it does, the line is sent with the credential replaced, since the
    /// transcript is read by anyone who can read the agent.
    pub fn read(&self, id: &str, from: Cursor, budget: usize) -> Option<Batch> {
        let path = self.claude.transcript(&claude::session_id(id))?;
        let secret = self.credential(id);
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
            let claude::Read {
                at,
                entries,
                waiting,
            } = claude::read(&String::from_utf8_lossy(&line));
            let at = at.filter(|at| instant(at)).unwrap_or_else(machine::now);
            let read: Vec<Line> = entries
                .into_iter()
                .zip(next.seq..)
                .map(|(entry, seq)| {
                    let mut line = entry.line(seq, at.clone());
                    if let Some(secret) = &secret {
                        if line.text.contains(secret.as_str()) {
                            line.text = line.text.replace(secret.as_str(), REDACTED);
                        }
                    }
                    line
                })
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
            next.waiting = waiting.unwrap_or(next.waiting);
            lines.extend(read);
        }
        (next.offset > from.offset).then_some(Batch { lines, next })
    }
}

/// How long a message may take to reach its agent before it is reported
/// failed: well inside the five minutes after which the service withdraws a
/// command its host has not reported, so the failure is still heard.
pub const PATIENCE: Duration = Duration::from_secs(120);

/// Messages carried to agents and not yet settled. Each is typed once its
/// session is listening, and is done only once the transcript shows the agent
/// was told it: a message typed into a terminal nobody is reading yet is
/// accepted by tmux and then lost. Held in memory, so a host that restarts
/// mid-delivery types a message the service sends again a second time.
pub struct Deliveries {
    held: Vec<Delivery>,
    patience: Duration,
}

struct Delivery {
    command: String,
    agent: String,
    text: String,
    since: Instant,
    /// Where the transcript ended when it was typed, once it has been.
    typed: Option<u64>,
}

impl Deliveries {
    pub fn new(patience: Duration) -> Self {
        Deliveries {
            held: Vec::new(),
            patience,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Whether `command` is already being delivered. The service sends a
    /// command again on every heartbeat until it is reported, and a message
    /// typed twice is said twice.
    pub fn holds(&self, command: &str) -> bool {
        self.held.iter().any(|d| d.command == command)
    }

    /// Takes a message to deliver, typing it at once if the agent is
    /// listening. What happened to it, if that is already known.
    pub fn hold(
        &mut self,
        agents: &Agents,
        command: &str,
        agent: &str,
        text: &str,
    ) -> Option<Result<(), String>> {
        if let Err(e) = agents.running(agent) {
            return Some(Err(e));
        }
        let mut delivery = Delivery {
            command: command.into(),
            agent: agent.into(),
            text: text.into(),
            since: Instant::now(),
            typed: None,
        };
        let done = delivery.step(agents, self.patience);
        if done.is_none() {
            self.held.push(delivery);
        }
        done
    }

    /// Moves every held message on, and hands back, by command, those that
    /// have reached their agent or never will.
    pub fn settle(&mut self, agents: &Agents) -> Vec<(String, Result<(), String>)> {
        let patience = self.patience;
        let mut settled = Vec::new();
        self.held.retain_mut(|d| match d.step(agents, patience) {
            Some(result) => {
                settled.push((d.command.clone(), result));
                false
            }
            None => true,
        });
        settled
    }
}

impl Delivery {
    fn step(&mut self, agents: &Agents, patience: Duration) -> Option<Result<(), String>> {
        if let Some(offset) = self.typed {
            if agents.told_since(&self.agent, offset, &self.text) {
                return Some(Ok(()));
            }
        }
        if let Err(e) = agents.running(&self.agent) {
            return Some(Err(e));
        }
        if self.typed.is_none() && agents.listening(&self.agent) {
            let offset = agents.end(&self.agent);
            if let Err(e) = agents.say(&self.agent, &self.text) {
                return Some(Err(e));
            }
            self.typed = Some(offset);
        }
        if self.since.elapsed() < patience {
            return None;
        }
        let waited = patience.as_secs();
        Some(Err(match self.typed {
            None => format!(
                "{} was not yet reading input after {waited}s, so the message was never typed",
                self.agent
            ),
            Some(_) => format!(
                "typed to {}, but not in its transcript after {waited}s: it may still take it \
                 at its next step",
                self.agent
            ),
        }))
    }
}

/// Whether `at` is an instant the service takes: UTC, as Claude Code writes it.
/// One it cannot take refuses the whole heartbeat, and every heartbeat after
/// it.
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
pwd > "$dir/$3.cwd"
w() { printf '%s\n' "$1" >> "$t"; }
w "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"$(printf '%s\n' "$4" | tail -n 1)\"}}"
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

    /// Stands in for Claude Code as it starts: what is typed before it is up
    /// is thrown away, as a terminal switched into raw mode discards it.
    const DEAF_CLAUDE: &str = r#"#!/bin/sh
dir="$CLAUDE_CONFIG_DIR/projects/-fake"; mkdir -p "$dir"; t="$dir/$3.jsonl"
w() { printf '%s\n' "$1" >> "$t"; }
sleep 1
stty -icanon min 0 time 0; cat > /dev/null; stty icanon
w '{"type":"user","message":{"role":"user","content":"wait to be told"}}'
w '{"type":"assistant","message":{"id":"m0","content":[{"type":"text","text":"waiting"}],"stop_reason":"end_turn"}}'
n=0
while IFS= read -r line; do
  n=$((n+1))
  w "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"$line\"}}"
  w "{\"type\":\"assistant\",\"message\":{\"id\":\"m$n\",\"content\":[{\"type\":\"text\",\"text\":\"on $line\"}],\"stop_reason\":\"end_turn\"}}"
done
"#;

    /// Never gets far enough to write a transcript.
    const STUCK_CLAUDE: &str = "#!/bin/sh\nsleep 60\n";

    /// Answers once and then never reads what is typed to it.
    const AWAY_CLAUDE: &str = r#"#!/bin/sh
dir="$CLAUDE_CONFIG_DIR/projects/-fake"; mkdir -p "$dir"; t="$dir/$3.jsonl"
printf '%s\n' '{"type":"assistant","message":{"id":"m0","content":[{"type":"text","text":"busy"}],"stop_reason":"tool_use"}}' >> "$t"
sleep 60
"#;

    /// Records the fleet's part of its environment, then waits.
    const FLEET_CLAUDE: &str = r#"#!/bin/sh
dir="$CLAUDE_CONFIG_DIR/projects/-fake"; mkdir -p "$dir"
printf '%s %s %s' "$FLEET_URL" "$AGENT_ID" "${FLEET_TOKEN-unset}" > "$dir/$3.fleet"
sleep 60
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
        host_with(name, program, BTreeMap::new())
    }

    fn host_with(name: &str, program: &str, repositories: BTreeMap<String, PathBuf>) -> Host {
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
                repositories,
                "http://fleet.test",
            ),
            dir,
        }
    }

    fn brief(text: &str) -> Placement<'_> {
        Placement {
            brief: Some(text),
            ..Default::default()
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
        agents.start("a1", &brief("fix the build")).unwrap();
        assert_eq!(agents.ids(), ["a1"]);
        let (mut seen, mut cursor) = (vec![], Cursor::default());

        // Read while it is still working, before it has said anything final.
        read_until(agents, "a1", &mut seen, &mut cursor, |s| {
            s.contains(&said("step one"))
        });
        assert_eq!(seen, [told("fix the build"), said("step one")]);
        assert!(!cursor.waiting, "mid-turn");

        agents.say("a1", "use the release branch").unwrap();
        read_until(agents, "a1", &mut seen, &mut cursor, |s| {
            s.contains(&said("which branch?"))
        });
        assert!(cursor.waiting, "its turn ended on a question");
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
        let command: crate::heartbeat::Command = serde_json::from_str(
            r#"{"id":"c1","kind":"start","agentId":"a1","workItemId":null,"role":"worker","level":1,"createdBy":null}"#,
        )
        .unwrap();
        let report =
            crate::heartbeat::take(&command, &host.agents, &mut Deliveries::new(PATIENCE)).unwrap();
        assert!(report.ok, "{report:?}");
        assert_eq!(host.agents.ids(), ["a1"]);
        assert!(host.dir.join("agents/a1").is_dir());
        let (mut seen, mut cursor) = (vec![], Cursor::default());
        read_until(&host.agents, "a1", &mut seen, &mut cursor, |s| {
            s.contains(&said("step one"))
        });
        assert_eq!(
            seen[0],
            told("Nothing has been asked of you yet. Wait to be told what the work is."),
            "started with nothing to do but wait"
        );
    }

    #[test]
    fn an_interrupt_stops_the_turn_and_the_session_goes_on() {
        let host = host("interrupt", BUSY_CLAUDE);
        let agents = &host.agents;
        agents.start("a1", &Placement::default()).unwrap();
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

    /// Settles `deliveries` until nothing is held, and says how each ended.
    fn settle_all(
        agents: &Agents,
        deliveries: &mut Deliveries,
    ) -> Vec<(String, Result<(), String>)> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut settled = vec![];
        while !deliveries.is_empty() {
            assert!(Instant::now() < deadline, "never settled");
            settled.extend(deliveries.settle(agents));
            thread::sleep(Duration::from_millis(50));
        }
        settled
    }

    fn command(json: &str) -> crate::heartbeat::Command {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn a_message_taken_with_its_agents_start_waits_until_it_listens() {
        let host = host("deaf", DEAF_CLAUDE);
        let agents = &host.agents;
        let mut deliveries = Deliveries::new(PATIENCE);
        // One heartbeat's commands, taken one after the other.
        let start = command(r#"{"id":"c1","kind":"start","agentId":"a1"}"#);
        let message = command(r#"{"id":"c2","kind":"message","agentId":"a1","text":"hello"}"#);
        let started = crate::heartbeat::take(&start, agents, &mut deliveries).unwrap();
        assert!(started.ok, "{started:?}");
        assert_eq!(
            crate::heartbeat::take(&message, agents, &mut deliveries),
            None,
            "not reported while it cannot yet be heard"
        );
        assert_eq!(
            crate::heartbeat::take(&message, agents, &mut deliveries),
            None,
            "sent again by the service, it is not typed twice"
        );

        assert_eq!(
            settle_all(agents, &mut deliveries),
            [("c2".to_string(), Ok(()))]
        );
        let (mut seen, mut cursor) = (vec![], Cursor::default());
        read_until(agents, "a1", &mut seen, &mut cursor, |s| {
            s.contains(&said("on hello"))
        });
        assert_eq!(seen.iter().filter(|e| **e == told("hello")).count(), 1);
    }

    #[test]
    fn a_message_is_done_only_once_it_is_in_the_transcript() {
        let host = host("delivered", FAKE_CLAUDE);
        let agents = &host.agents;
        agents.start("a1", &brief("fix the build")).unwrap();
        let (mut seen, mut cursor) = (vec![], Cursor::default());
        read_until(agents, "a1", &mut seen, &mut cursor, |s| {
            s.contains(&said("step one"))
        });

        // Typed at once, but taken only at its next step, a second from now.
        let mut deliveries = Deliveries::new(PATIENCE);
        assert_eq!(deliveries.hold(agents, "c1", "a1", "use main"), None);
        assert_eq!(deliveries.settle(agents), []);
        let end = agents.end("a1");
        assert!(!agents.told_since("a1", 0, "use main"), "not there yet");

        assert_eq!(
            settle_all(agents, &mut deliveries),
            [("c1".to_string(), Ok(()))]
        );
        assert!(
            agents.told_since("a1", end, "use main"),
            "in the transcript when it was reported"
        );
    }

    #[test]
    fn a_message_that_cannot_be_delivered_in_time_fails_with_why() {
        let patience = Duration::from_millis(300);

        let stuck = host("stuck", STUCK_CLAUDE);
        stuck.agents.start("a1", &Placement::default()).unwrap();
        let mut deliveries = Deliveries::new(patience);
        assert_eq!(deliveries.hold(&stuck.agents, "c1", "a1", "hello"), None);
        let [(_, Err(why))] = &settle_all(&stuck.agents, &mut deliveries)[..] else {
            panic!("expected one failure")
        };
        assert!(why.contains("never typed"), "{why}");

        let away = host("away", AWAY_CLAUDE);
        away.agents.start("a1", &Placement::default()).unwrap();
        let (mut seen, mut cursor) = (vec![], Cursor::default());
        read_until(&away.agents, "a1", &mut seen, &mut cursor, |s| {
            !s.is_empty()
        });
        let mut deliveries = Deliveries::new(patience);
        assert_eq!(deliveries.hold(&away.agents, "c1", "a1", "hello"), None);
        let [(_, Err(why))] = &settle_all(&away.agents, &mut deliveries)[..] else {
            panic!("expected one failure")
        };
        assert!(why.contains("not in its transcript"), "{why}");
    }

    #[test]
    fn a_start_already_done_is_not_done_again() {
        let host = host("again", FAKE_CLAUDE);
        host.agents.start("a1", &brief("fix the build")).unwrap();
        // A host that restarts has nothing but tmux and the transcripts.
        let restarted = Agents::new(
            Tmux::new(format!("agent-host-test-{}-again", std::process::id())),
            Claude::new(
                host.dir.join("claude").to_string_lossy(),
                Some(host.dir.join("state")),
                "/nonexistent".into(),
            ),
            host.dir.join("agents"),
            BTreeMap::new(),
            "http://fleet.test",
        );
        restarted.start("a1", &brief("fix the build")).unwrap();
        assert_eq!(restarted.ids(), ["a1"]);

        // One that has run and ended left its transcript behind.
        let (mut seen, mut cursor) = (vec![], Cursor::default());
        read_until(&restarted, "a1", &mut seen, &mut cursor, |s| !s.is_empty());
        restarted.tmux.kill_server();
        restarted.start("a1", &brief("fix the build")).unwrap();
        assert!(restarted.ids().is_empty(), "started a finished agent again");
    }

    #[test]
    fn a_start_that_cannot_run_says_why() {
        let host = host("missing", FAKE_CLAUDE);
        let agents = Agents::new(
            Tmux::new(format!("agent-host-test-{}-missing", std::process::id())),
            Claude::new("/nonexistent/claude", None, "/nonexistent".into()),
            host.dir.join("agents"),
            BTreeMap::new(),
            "http://fleet.test",
        );
        let err = agents.start("a1", &Placement::default()).unwrap_err();
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

    #[test]
    fn an_agent_whose_turn_ended_waits_until_it_is_told_something() {
        let host = host("turns", FAKE_CLAUDE);
        let path = transcript(&host, "a4");
        let cursor = std::cell::Cell::new(Cursor::default());
        let read = |lines: &[&str]| {
            for line in lines {
                append(&path, format!("{line}\n").as_bytes());
            }
            cursor.set(host.agents.read("a4", cursor.get(), 1 << 16).unwrap().next);
            cursor.get().waiting
        };
        assert!(
            !read(&[
                r#"{"type":"user","message":{"role":"user","content":"fix the build"}}"#,
                r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","name":"Bash","input":{}}],"stop_reason":"tool_use"}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}"#,
            ]),
            "mid-turn"
        );
        assert!(
            read(&[
                r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"thinking","thinking":""}],"stop_reason":null}}"#,
                r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"Which branch?"}],"stop_reason":"end_turn"}}"#,
                r#"{"type":"system","subtype":"turn_duration","durationMs":1}"#,
            ]),
            "its turn ended and nothing has been said since"
        );
        assert!(
            host.agents.read("a4", cursor.get(), 1 << 16).is_none(),
            "nothing new: the heartbeat keeps what the cursor says"
        );
        assert!(
            !read(&[r#"{"type":"user","message":{"role":"user","content":"main"}}"#,]),
            "told something, it is at work again"
        );
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
                seq: 0,
                waiting: false
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

    fn repo_host(name: &str) -> (Host, crate::workspace::tests::Repo) {
        let repo = crate::workspace::tests::repo(&format!("agent-{name}"));
        let repositories = BTreeMap::from([("widgets".to_string(), repo.checkout.clone())]);
        (host_with(name, FAKE_CLAUDE, repositories), repo)
    }

    /// Where the stand-in for Claude Code found itself started.
    fn cwd(host: &Host, id: &str) -> PathBuf {
        let file = host.dir.join(format!(
            "state/projects/-fake/{}.cwd",
            claude::session_id(id)
        ));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !file.exists() {
            assert!(Instant::now() < deadline, "never started");
            thread::sleep(Duration::from_millis(50));
        }
        PathBuf::from(fs::read_to_string(file).unwrap().trim())
    }

    #[test]
    fn an_agent_placed_for_a_repository_starts_in_a_worktree_of_its_own() {
        let (host, _repo) = repo_host("worktree");
        let placed = |text| Placement {
            repository: Some("widgets"),
            brief: Some(text),
            ..Default::default()
        };
        host.agents.start("a1", &placed("fix the build")).unwrap();
        host.agents.start("a2", &placed("write the docs")).unwrap();

        let (one, two) = (cwd(&host, "a1"), cwd(&host, "a2"));
        assert_ne!(one, two, "two agents, two trees");
        for (id, dir) in [("a1", &one), ("a2", &two)] {
            assert!(dir.join("README").is_file(), "the code is there");
            let branch = std::process::Command::new("git")
                .args(["branch", "--show-current"])
                .current_dir(dir)
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&branch.stdout).trim(),
                format!("agent/{id}")
            );
        }

        let prompt = fs::read_to_string(host.dir.join("agents/a1/prompt.md")).unwrap();
        assert!(prompt.starts_with(CONDUCT.split("{{WORKSPACE}}").next().unwrap()));
        assert!(
            prompt.contains("## Push your branch"),
            "the rules come first"
        );
        assert!(prompt.contains("on branch `agent/a1`"), "{prompt}");
        assert!(
            prompt.ends_with("\nfix the build"),
            "and the brief after them, as placed"
        );
        let (mut seen, mut cursor) = (vec![], Cursor::default());
        read_until(&host.agents, "a1", &mut seen, &mut cursor, |s| {
            !s.is_empty()
        });
        assert_eq!(seen[0], told("fix the build"));
    }

    #[test]
    fn a_stop_ends_the_session_and_leaves_its_work_where_it_is() {
        let (host, repo) = repo_host("stop");
        let agents = &host.agents;
        agents
            .start(
                "a1",
                &Placement {
                    repository: Some("widgets"),
                    ..Default::default()
                },
            )
            .unwrap();
        let tree = cwd(&host, "a1");
        crate::workspace::tests::sh(&tree, "echo half-done > WORK");
        let stop = command(r#"{"id":"c1","kind":"stop","agentId":"a1"}"#);
        let mut deliveries = Deliveries::new(PATIENCE);

        let report = crate::heartbeat::take(&stop, agents, &mut deliveries).unwrap();
        assert!(report.ok, "{report:?}");
        assert!(agents.ids().is_empty(), "the next heartbeat lists no agent");
        assert_eq!(
            fs::read_to_string(tree.join("WORK")).unwrap(),
            "half-done\n"
        );
        let branch = std::process::Command::new("git")
            .args(["branch", "--list", "agent/a1"])
            .current_dir(&repo.checkout)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&branch.stdout).contains("agent/a1"),
            "the branch is kept"
        );

        let again = crate::heartbeat::take(&stop, agents, &mut deliveries).unwrap();
        assert!(again.ok, "stopping one not running is done: {again:?}");
    }

    #[test]
    fn a_brief_too_long_for_tmux_reaches_the_agent_whole() {
        let (host, _repo) = repo_host("long");
        let long = "x".repeat(60 << 10) + "\nthe end";
        host.agents
            .start(
                "a1",
                &Placement {
                    repository: Some("widgets"),
                    brief: Some(&long),
                    ..Default::default()
                },
            )
            .unwrap();
        let (mut seen, mut cursor) = (vec![], Cursor::default());
        read_until(&host.agents, "a1", &mut seen, &mut cursor, |s| {
            !s.is_empty()
        });
        assert_eq!(seen[0], told("the end"));
    }

    #[test]
    fn a_workspace_that_cannot_be_made_fails_the_start_and_runs_nothing() {
        let (host, repo) = repo_host("unprepared");
        let err = host
            .agents
            .start(
                "a1",
                &Placement {
                    repository: Some("gadgets"),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(err.contains("no checkout of gadgets"), "{err}");

        crate::workspace::tests::sh(&repo.checkout, "git remote set-url origin /nonexistent");
        let err = host
            .agents
            .start(
                "a2",
                &Placement {
                    repository: Some("widgets"),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(err.contains("could not fetch from origin"), "{err}");

        let err = host
            .agents
            .start(
                "a3",
                &Placement {
                    branch: Some("feat/x"),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(err.contains("no repository"), "{err}");

        let err = host
            .agents
            .start("a4", &brief(&"x".repeat(LONGEST_PROMPT)))
            .unwrap_err();
        assert!(err.contains("more than"), "{err}");
        assert!(host.agents.ids().is_empty(), "no agent was started anyway");
    }

    #[test]
    fn the_rules_say_how_to_act_on_the_fleet_by_role_and_never_with_a_token() {
        for heading in [
            "## The fleet, as yourself",
            "## If an agent placed you",
            "## If your role is `lead`",
        ] {
            assert!(CONDUCT.contains(heading), "{heading}");
        }
        let blocks: Vec<&str> = CONDUCT
            .split("```sh\n")
            .skip(1)
            .map(|b| b.split("```").next().unwrap())
            .collect();
        assert!(blocks.len() >= 4, "{blocks:?}");
        for block in blocks {
            let curls = block.matches("curl ").count();
            assert!(curls > 0, "{block}");
            assert_eq!(
                block
                    .matches(r#"-H "Authorization: Bearer $FLEET_TOKEN""#)
                    .count(),
                curls,
                "every call is as the agent itself: {block}"
            );
            assert!(block.contains("$FLEET_URL/fleet/"), "{block}");
            assert!(!block.contains("agt_"), "{block}");
            let syntax = std::process::Command::new("sh")
                .args(["-n", "-c", block])
                .output()
                .unwrap();
            assert!(syntax.status.success(), "{block}: {syntax:?}");
        }
    }

    /// What the stand-in for Claude Code found in its environment.
    fn fleet_env(host: &Host, id: &str) -> String {
        let file = host.dir.join(format!(
            "state/projects/-fake/{}.fleet",
            claude::session_id(id)
        ));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !file.exists() {
            assert!(Instant::now() < deadline, "never started");
            thread::sleep(Duration::from_millis(50));
        }
        thread::sleep(Duration::from_millis(100));
        fs::read_to_string(file).unwrap()
    }

    #[test]
    fn the_first_credential_given_is_the_one_kept() {
        let host = host("credential", FLEET_CLAUDE);
        let (first, later) = (
            Credential("agt_first".into()),
            Credential("agt_later".into()),
        );
        let err = host
            .agents
            .start(
                "a1",
                &Placement {
                    branch: Some("feat/x"),
                    credential: Some(&first),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(!err.contains("agt_first"), "{err}");
        host.agents
            .start(
                "a1",
                &Placement {
                    credential: Some(&later),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(fleet_env(&host, "a1"), "http://fleet.test a1 agt_first");
        assert_eq!(format!("{:?}", Placement {
            credential: Some(&first),
            ..Default::default()
        }), "Placement { repository: None, branch: None, brief: None, credential: Some(Credential(..)) }");
    }

    #[test]
    fn an_agent_started_without_a_credential_holds_none() {
        let host = host("no-credential", FLEET_CLAUDE);
        host.agents
            .tmux
            .start("other", Path::new("/"), &["sleep".into(), "60".into()], &[])
            .unwrap();
        // Whatever the tmux server was started with, the agent has no token
        // that is not its own.
        std::process::Command::new("tmux")
            .args([
                "-L",
                &format!("agent-host-test-{}-no-credential", std::process::id()),
            ])
            .args(["set-environment", "-g", "FLEET_TOKEN", "someone-elses"])
            .status()
            .unwrap();
        host.agents.start("a1", &Placement::default()).unwrap();
        assert_eq!(fleet_env(&host, "a1"), "http://fleet.test a1 unset");
    }

    /// Against Claude Code itself, which costs a real session:
    /// `cargo test -- --ignored real_claude`.
    #[test]
    #[ignore]
    fn real_claude() {
        let dir = std::env::temp_dir().join(format!("agent-host-real-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let tmux = Tmux::new(format!("agent-host-test-real-{}", std::process::id()));
        let agents = Agents::new(
            tmux,
            Claude::from_env(|k| std::env::var(k).ok()),
            dir,
            BTreeMap::new(),
            "http://fleet.test",
        );
        let id = format!("real-{}", std::process::id());
        agents
            .start(
                &id,
                &brief(
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

    /// Claude Code itself, placed for a repository, reading what only that
    /// repository holds: `cargo test -- --ignored real_claude`.
    #[test]
    #[ignore]
    fn real_claude_sees_its_repository() {
        let repo = crate::workspace::tests::repo("real");
        crate::workspace::tests::sh(
            &repo.checkout,
            "echo 'The password is periwinkle.' > SECRET && git add SECRET && git commit -qm secret \
             && git push -q origin main",
        );
        let dir = repo.root.join("agents");
        let tmux = Tmux::new(format!("agent-host-test-real-w-{}", std::process::id()));
        let agents = Agents::new(
            tmux,
            Claude::from_env(|k| std::env::var(k).ok()),
            dir.clone(),
            BTreeMap::from([("widgets".to_string(), repo.checkout.clone())]),
            "http://fleet.test",
        );
        let id = format!("real-w-{}", std::process::id());
        agents
            .start(
                &id,
                &Placement {
                    repository: Some("widgets"),
                    brief: Some(
                        "Read the file SECRET in your working directory, and run `git branch \
                         --show-current`. Then reply with one line: the password, a space, and \
                         the branch.",
                    ),
                    ..Default::default()
                },
            )
            .unwrap();
        let (mut seen, mut cursor): (Seen, _) = (vec![], Cursor::default());
        let deadline = Instant::now() + Duration::from_secs(180);
        // Nothing says it has stopped, so it is done once it has said the
        // answer: what only the repository holds, on its own branch.
        let answered = |s: &Seen| {
            s.iter().any(|(from, text)| {
                *from == "agent"
                    && text.contains("periwinkle")
                    && text.contains(&format!("agent/{id}"))
            })
        };
        while !answered(&seen) {
            assert!(
                Instant::now() < deadline,
                "it never read the repository on its own branch; read {seen:?}"
            );
            if let Some(batch) = agents.read(&id, cursor, 1 << 16) {
                take(&mut seen, &mut cursor, batch);
            }
            thread::sleep(Duration::from_millis(500));
        }
        agents.tmux.kill_server();
        eprintln!("{seen:#?}");
    }

    /// Interrupting Claude Code itself, mid-command, as the page does:
    /// `cargo test -- --ignored real_claude`.
    #[test]
    #[ignore]
    fn real_claude_interrupted() {
        let dir = std::env::temp_dir().join(format!("agent-host-real-i-{}", std::process::id()));
        let tmux = Tmux::new(format!("agent-host-test-real-i-{}", std::process::id()));
        let agents = Agents::new(
            tmux,
            Claude::from_env(|k| std::env::var(k).ok()),
            dir,
            BTreeMap::new(),
            "http://fleet.test",
        );
        let id = format!("real-i-{}", std::process::id());
        agents.start(&id, &Placement::default()).unwrap();
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
