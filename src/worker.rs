//! The workers on this host. Each has a folder of its own (home.rs), and
//! while it has a session, a Claude Code running in tmux, which takes input
//! while it works and writes its transcript as it goes. The host remembers
//! nothing about them that their folders and tmux do not hold.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use std::collections::BTreeMap;

use crate::claude::{self, Begin, Claude};
use crate::context::{self, Context};
use crate::home::{self, Home, Record, Session};
use crate::protocol::{Line, State};
use crate::sys;
use crate::tmux::Tmux;

/// The opening message is one argument to Claude Code, and linux takes no
/// single argument longer than this.
const LONGEST_PROMPT: usize = 128 << 10;

/// What a new session is told when it was started with nothing to say. Claude
/// Code writes nothing until it is told something, and a message is typed only
/// once it has answered, so a session always opens with words.
const NOTHING_YET: &str = "Nothing has been asked of you yet. Wait to be told what the work is.";

/// What a session that carries on the last conversation is told when it was
/// resumed with nothing to say.
const CARRY_ON: &str = "Your last session ended and this one carries on from it. If you were \
                        waiting to be told something, keep waiting; otherwise carry on where you \
                        left off.";

/// However much has piled up, a read stops after this many bytes of the raw
/// transcript, most of which is Claude Code's own bookkeeping.
const RAW_LIMIT: u64 = 8 << 20;

/// The most lines the service takes for one worker in one report.
const MOST_LINES: usize = 1000;

/// Between the key that stops a turn and the text that follows it: sent
/// together, the terminal reads the two as one escape sequence, and the text
/// lands while the turn is still stopping.
const SETTLE: Duration = Duration::from_millis(500);

/// One step of a worker's conversation, as its transcript says it.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// Its opening message, or a message carried to it.
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

impl Entry {
    fn line(self, seq: u64, at: String, session_id: String) -> Line {
        let (from, text) = match self {
            Entry::Told { text } => ("instruction", text),
            Entry::Said { text } => ("agent", text),
            Entry::Ran { tool, input } => ("tool", format!("{tool} {input}")),
        };
        Line {
            seq,
            at,
            session_id,
            from,
            text,
        }
    }
}

/// How far into a worker's transcripts has been accounted for: which of them
/// (see `transcripts`), the byte after the last whole line read in it, the
/// `seq` of the line that comes next, and whether, as of that line, the
/// worker's last turn has ended with nothing said to it since.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Cursor {
    pub file: usize,
    pub offset: u64,
    pub seq: u64,
    pub waiting: bool,
}

/// The lines read from a worker's transcripts, and where the read after this
/// one starts. A batch with no lines still moves the cursor past what it read:
/// those bytes were read and found to say nothing.
#[derive(Debug)]
pub struct Batch {
    pub lines: Vec<Line>,
    pub next: Cursor,
}

/// The Claude Code transcripts a worker's sessions wrote to, in the order
/// they were begun, each once: a session that carries on the last
/// conversation writes on in the transcript before it.
fn transcripts(sessions: &[Session]) -> Vec<&str> {
    let mut files: Vec<&str> = Vec::new();
    for s in sessions {
        if !files.contains(&s.claude.as_str()) {
            files.push(&s.claude);
        }
    }
    files
}

pub struct Workers {
    tmux: Tmux,
    claude: Claude,
    dir: PathBuf,
}

impl Workers {
    pub fn new(tmux: Tmux, claude: Claude, dir: PathBuf) -> Self {
        Workers { tmux, claude, dir }
    }

    /// Every worker here that is not retired.
    pub fn all(&self) -> Vec<(Home, Record)> {
        home::all(&self.dir)
    }

    fn find(&self, id: &str) -> Result<(Home, Record), String> {
        self.all()
            .into_iter()
            .find(|(_, r)| r.id == id)
            .ok_or_else(|| {
                if home::retired(&self.dir, id) {
                    format!("worker {id} is retired")
                } else {
                    format!("no worker {id} on this host")
                }
            })
    }

    /// Makes the worker's folder: its record and a Claude Code state of its
    /// own. A create is sent again until it is reported, and one the host has
    /// done already is taken up where it stands.
    pub fn create(&self, id: &str, name: &str) -> Result<(), String> {
        if !home::valid_name(name) {
            return Err(format!(
                "{name:?} is not a worker name: [a-z0-9][a-z0-9-]{{0,39}}"
            ));
        }
        if home::retired(&self.dir, id) {
            return Err(format!("worker {id} was retired here"));
        }
        if let Some((_, held)) = self.all().into_iter().find(|(_, r)| r.id == id) {
            if held.name != name {
                return Err(format!("worker {id} is here already, named {}", held.name));
            }
        }
        let home = Home::new(self.dir.join(name));
        let record = Record {
            id: id.into(),
            name: name.into(),
        };
        home.make(&record)?;
        // A create sent again keeps the context a start has since written.
        let rules = context::rules(
            &rules(&home, &record),
            context::written(&home.claude()).as_ref(),
        );
        self.claude.prepare(&home.claude(), &home.dir, &rules)
    }

    /// Runs a session for the worker, in tmux, in its folder: a new
    /// conversation, or with `resume` the worker's last one carried on.
    /// `text` is what it is told first.
    ///
    /// A start is sent again until it is reported, so one the host has done
    /// already -- before a restart that lost the report -- is done, not
    /// repeated: its session is running, or it ran and has ended since.
    #[cfg(test)]
    pub fn start(
        &self,
        id: &str,
        session_id: &str,
        resume: bool,
        text: Option<&str>,
    ) -> Result<(), String> {
        self.start_with(id, session_id, resume, text, Given::default())
    }

    /// `start`, with what the start carries besides its message: a context
    /// written into the worker's Claude Code folder first, and variables for
    /// its session.
    pub fn start_with(
        &self,
        id: &str,
        session_id: &str,
        resume: bool,
        text: Option<&str>,
        given: Given<'_>,
    ) -> Result<(), String> {
        let (home, record) = self.find(id)?;
        let sessions = home.sessions();
        if self.session(id).is_some() {
            return match sessions.last() {
                Some(s) if s.session_id == session_id => Ok(()),
                Some(s) => Err(format!(
                    "{} already has session {} running: stop it first",
                    record.name, s.session_id
                )),
                None => Err(format!(
                    "{} already has a session running that this host did not start",
                    record.name
                )),
            };
        }
        if sessions.iter().any(|s| s.session_id == session_id) {
            return Ok(());
        }
        self.claude.installed()?;
        let previous = sessions.last().filter(|_| resume);
        let session = match previous {
            Some(p) => Session {
                session_id: session_id.into(),
                claude: p.claude.clone(),
                from: claude::transcript(&home.claude(), &p.claude)
                    .and_then(|path| fs::metadata(path).ok())
                    .map_or(0, |m| m.len()),
                stopped: false,
            },
            None => Session {
                session_id: session_id.into(),
                claude: claude::session_id(session_id),
                from: 0,
                stopped: false,
            },
        };
        let begin = match previous {
            Some(_) => Begin::Continue,
            None => Begin::Fresh(&session.claude),
        };
        let text = text.unwrap_or(match begin {
            Begin::Continue => CARRY_ON,
            Begin::Fresh(_) => NOTHING_YET,
        });
        if text.len() > LONGEST_PROMPT {
            return Err(format!(
                "the opening message is {} bytes, more than the {LONGEST_PROMPT} one argument can \
                 carry",
                text.len()
            ));
        }
        let prompt = home.prompt();
        fs::write(&prompt, text).map_err(|e| format!("{}: {e}", prompt.display()))?;
        // A start without a context keeps the last one the worker was given.
        let written = match given.context {
            Some(c) => Some(context::write(&home.claude(), c)?),
            None => context::written(&home.claude()),
        };
        // The rules follow the host's version, so each session is told the
        // ones of the host it runs on, then the context's.
        let rules = context::rules(&rules(&home, &record), written.as_ref());
        self.claude.prepare(&home.claude(), &home.dir, &rules)?;
        let argv = self.claude.argv(begin, &prompt);
        let mut env = self.claude.env(&home.claude());
        env.extend(context::env(given.env));
        // Written before tmux is asked, so a machine that dies between the two
        // leaves a session it knows of, which then reads as interrupted.
        home.begin(&session)?;
        if let Err(e) = self.tmux.start(
            id,
            &format!("worker-{}", record.name),
            &home.dir,
            &argv,
            &env,
            Some(&home.log(session_id)),
        ) {
            let _ = home.withdraw(session_id);
            return Err(e);
        }
        Ok(())
    }

    /// Every live session, by the id of the worker it is for, mapped to
    /// tmux's id for it.
    pub fn live(&self) -> HashMap<String, String> {
        self.tmux.sessions().into_iter().collect()
    }

    fn session(&self, id: &str) -> Option<String> {
        self.live().remove(id)
    }

    fn running(&self, id: &str) -> Result<String, String> {
        self.session(id)
            .ok_or_else(|| format!("worker {id} has no live session on this host"))
    }

    /// Takes up every live session again, as a host does once it has
    /// restarted: each one's terminal goes on being logged. What is not live
    /// needs nothing: a session that ended without a stop already reads as
    /// interrupted.
    pub fn reconcile(&self) -> Vec<(String, State)> {
        let live = self.live();
        self.all()
            .into_iter()
            .map(|(home, record)| {
                let tmux = live.get(&record.id);
                if let (Some(tmux), Some(latest)) = (tmux, home.latest()) {
                    let _ = self.tmux.log_to(tmux, &home.log(&latest.session_id));
                }
                let (state, _) = self.state(&home, tmux.map(String::as_str), false);
                (record.name, state)
            })
            .collect()
    }

    /// What the worker is doing. `live` is its tmux session, if it has one,
    /// and `waiting` what its transcript says of its last turn.
    pub fn state(&self, home: &Home, live: Option<&str>, waiting: bool) -> (State, Option<String>) {
        let latest = home.latest();
        match live {
            Some(_) => {
                let state = if waiting {
                    State::Waiting
                } else {
                    State::Working
                };
                (state, latest.map(|s| s.session_id))
            }
            None => match latest {
                Some(s) if !s.stopped => (State::Interrupted, Some(s.session_id)),
                _ => (State::Idle, None),
            },
        }
    }

    /// Types `text` into the worker's session now, whether it is working or
    /// waiting. Working, it takes the text at its next step rather than when
    /// it has finished; waiting on a question, the text is the answer and the
    /// same session goes on. A message carried for the service goes through
    /// `Deliveries`, which waits until the session is listening.
    pub fn say(&self, id: &str, text: &str) -> Result<(), String> {
        self.tmux.type_in(&self.running(id)?, text)
    }

    /// Stops the turn the worker is in, as Escape does for a person at its
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

    /// Ends the worker's session, and Claude Code with it; the worker is idle.
    /// Its folder is left as it is. One with no
    /// session, or not here at all, is already stopped, which is what was
    /// asked; one whose session ended without a stop is idle from now on.
    pub fn stop(&self, id: &str) -> Result<(), String> {
        let Ok((home, _)) = self.find(id) else {
            return Ok(());
        };
        if let Some(session) = self.session(id) {
            self.tmux.kill_session(&session)?;
        }
        match home.latest() {
            Some(latest) if !latest.stopped => home.stopped(&latest.session_id),
            _ => Ok(()),
        }
    }

    /// Stops the worker and moves its folder aside, where it is kept (see
    /// `Home::retire`). One not here, or retired already, is done.
    pub fn retire(&self, id: &str) -> Result<(), String> {
        let Ok((home, record)) = self.find(id) else {
            return Ok(());
        };
        self.stop(id)?;
        home.retire(&record)
    }

    /// Whether Claude Code has answered anything in the worker's latest
    /// session. Until it has, it may still be starting, and what is typed into
    /// its terminal then is lost without a trace.
    fn listening(&self, id: &str) -> bool {
        let Ok((home, _)) = self.find(id) else {
            return false;
        };
        let Some(latest) = home.latest() else {
            return false;
        };
        self.scan(&home, &latest, latest.from, |entries| {
            entries
                .iter()
                .any(|e| matches!(e, Entry::Said { .. } | Entry::Ran { .. }))
        })
    }

    /// Where the latest session's transcript ends now, so what is written
    /// after can be told apart.
    fn end(&self, id: &str) -> u64 {
        self.find(id)
            .ok()
            .and_then(|(home, _)| {
                let latest = home.latest()?;
                claude::transcript(&home.claude(), &latest.claude)
            })
            .and_then(|path| fs::metadata(path).ok())
            .map_or(0, |m| m.len())
    }

    /// Whether the worker has been told `text` since `offset` in its latest
    /// session's transcript, as typed or as Claude Code trimmed it.
    fn told_since(&self, id: &str, offset: u64, text: &str) -> bool {
        let Ok((home, _)) = self.find(id) else {
            return false;
        };
        let Some(latest) = home.latest() else {
            return false;
        };
        let typed = [claude::carried(text), claude::carried(text.trim())];
        self.scan(&home, &latest, offset, |entries| {
            entries
                .iter()
                .any(|e| matches!(e, Entry::Told { text } if typed.contains(text)))
        })
    }

    /// Whether `found` holds of any whole line of the session's transcript
    /// after `offset`. A line cut through by `offset` reads as nothing.
    fn scan(
        &self,
        home: &Home,
        session: &Session,
        offset: u64,
        found: impl Fn(&[Entry]) -> bool,
    ) -> bool {
        let Some(mut file) = claude::transcript(&home.claude(), &session.claude)
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

    /// What the worker has done since `from`, in all its sessions, as much
    /// as fits in `budget` bytes of lines; the rest waits for the next read.
    /// None when nothing new has been written. Only whole lines are read from
    /// the transcript still being written, so a line Claude Code is in the
    /// middle of is left for next time; one an ended session left torn is
    /// passed over.
    pub fn read(&self, home: &Home, from: Cursor, budget: usize) -> Option<Batch> {
        let sessions = home.sessions();
        let files = transcripts(&sessions);
        let (mut next, mut lines, mut size) = (from, Vec::new(), 0);
        'files: while next.file < files.len() {
            let claude = files[next.file];
            let last = next.file + 1 == files.len();
            let file = claude::transcript(&home.claude(), claude).and_then(|p| File::open(p).ok());
            let Some(mut file) = file else {
                if last {
                    break;
                }
                next.file += 1;
                next.offset = 0;
                continue;
            };
            if file.seek(SeekFrom::Start(next.offset)).is_err() {
                break;
            }
            let ranges: Vec<&Session> = sessions.iter().filter(|s| s.claude == claude).collect();
            let mut raw = BufReader::new(file).take(RAW_LIMIT);
            let mut taken = 0;
            let mut line = Vec::new();
            loop {
                line.clear();
                let n = match raw.read_until(b'\n', &mut line) {
                    Ok(n) => n,
                    Err(_) => break 'files,
                };
                taken += n as u64;
                if n == 0 || (!line.ends_with(b"\n") && last) {
                    // The end of what there is, or of what one read takes.
                    if last || taken >= RAW_LIMIT {
                        break 'files;
                    }
                    next.file += 1;
                    next.offset = 0;
                    continue 'files;
                }
                let session = ranges
                    .iter()
                    .rev()
                    .find(|s| s.from <= next.offset)
                    .or(ranges.first())
                    .map_or("", |s| s.session_id.as_str());
                // Lossy, so a line that is not UTF-8 is passed over rather
                // than holding the cursor in front of it for ever.
                let claude::Read {
                    at,
                    entries,
                    waiting,
                } = claude::read(&String::from_utf8_lossy(&line));
                let at = at.filter(|at| instant(at)).unwrap_or_else(sys::now);
                let read: Vec<Line> = entries
                    .into_iter()
                    .zip(next.seq..)
                    .map(|(entry, seq)| entry.line(seq, at.clone(), session.into()))
                    .collect();
                let more: usize = read
                    .iter()
                    .map(|l| serde_json::to_string(l).map_or(0, |s| s.len()))
                    .sum();
                // The first line is always taken, so one too large for the
                // budget cannot hold up everything after it.
                if !lines.is_empty()
                    && (size + more > budget || lines.len() + read.len() > MOST_LINES)
                {
                    break 'files;
                }
                size += more;
                next.offset += line.len() as u64;
                next.seq += read.len() as u64;
                next.waiting = waiting.unwrap_or(next.waiting);
                lines.extend(read);
            }
        }
        (next != from).then_some(Batch { lines, next })
    }
}

/// What a start carries besides its message.
#[derive(Debug, Default, Clone, Copy)]
pub struct Given<'a> {
    pub context: Option<&'a Context>,
    pub env: Option<&'a BTreeMap<String, String>>,
}

/// The worker's standing rules, Claude Code's user memory.
fn rules(home: &Home, record: &Record) -> String {
    format!(
        "You are worker `{}`, and your folder is `{}`. A person reads everything you \
         write, and what to do comes to you as messages. Clone whatever you need into \
         your folder; `home/`, `logs/`, `prompt.md`, `sessions.jsonl` and `worker.json` \
         are the host's records of you, so leave them be.\n",
        record.name,
        home.dir.display()
    )
}

/// How long a message may take to reach its worker before it is reported
/// failed: well inside the five minutes after which a command not reported is
/// worth asking about, so the failure is still heard.
pub const PATIENCE: Duration = Duration::from_secs(120);

/// Messages carried to workers and not yet settled. Each is typed once its
/// session is listening, and is done only once the transcript shows the
/// worker was told it: a message typed into a terminal nobody is reading yet
/// is accepted by tmux and then lost. Held in memory, so a host that restarts
/// mid-delivery types a message the service sends again a second time.
pub struct Deliveries {
    held: Vec<Delivery>,
    patience: Duration,
}

struct Delivery {
    command: String,
    worker: String,
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

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Whether `command` is already being delivered. The service sends a
    /// command again until it is reported, and a message typed twice is said
    /// twice.
    pub fn holds(&self, command: &str) -> bool {
        self.held.iter().any(|d| d.command == command)
    }

    /// Takes a message to deliver, typing it at once if the worker is
    /// listening. What happened to it, if that is already known.
    pub fn hold(
        &mut self,
        workers: &Workers,
        command: &str,
        worker: &str,
        text: &str,
    ) -> Option<Result<(), String>> {
        if let Err(e) = workers.running(worker) {
            return Some(Err(e));
        }
        let mut delivery = Delivery {
            command: command.into(),
            worker: worker.into(),
            text: text.into(),
            since: Instant::now(),
            typed: None,
        };
        let done = delivery.step(workers, self.patience);
        if done.is_none() {
            self.held.push(delivery);
        }
        done
    }

    /// Moves every held message on, and hands back, by command, those that
    /// have reached their worker or never will.
    pub fn settle(&mut self, workers: &Workers) -> Vec<(String, Result<(), String>)> {
        let patience = self.patience;
        let mut settled = Vec::new();
        self.held.retain_mut(|d| match d.step(workers, patience) {
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
    fn step(&mut self, workers: &Workers, patience: Duration) -> Option<Result<(), String>> {
        if let Some(offset) = self.typed {
            if workers.told_since(&self.worker, offset, &self.text) {
                return Some(Ok(()));
            }
        }
        if let Err(e) = workers.running(&self.worker) {
            return Some(Err(e));
        }
        if self.typed.is_none() && workers.listening(&self.worker) {
            let offset = workers.end(&self.worker);
            if let Err(e) = workers.say(&self.worker, &self.text) {
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
                "worker {} was not yet reading input after {waited}s, so the message was never \
                 typed",
                self.worker
            ),
            Some(_) => format!(
                "typed to worker {}, but not in its transcript after {waited}s: it may still take \
                 it at its next step",
                self.worker
            ),
        }))
    }
}

/// Whether `at` is an instant the service takes: UTC, as Claude Code writes it.
/// One it cannot take refuses the whole report, and every report after it.
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
#[path = "worker_tests.rs"]
pub mod tests;
