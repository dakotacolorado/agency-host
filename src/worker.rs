//! The workers on this host. Each has a folder of its own (home.rs), and
//! while it has a session, a headless Claude Code (session.rs), which takes
//! messages while it works and says what it does as it goes. The host
//! remembers nothing about them that their folders do not hold.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;

use crate::claude::{self, Begin, Claude};
use crate::context::{self, Context};
use crate::home::{self, Home, Record, Session};
use crate::protocol::{Line, State, Usage, WorkerReport};
use crate::session::{Echoes, Sessions, Spawn};
use crate::sys;

/// What a new session is told when it was started with nothing to say. Claude
/// Code does nothing until it is told something, so a session always opens
/// with words.
const NOTHING_YET: &str = "Nothing has been asked of you yet. Wait to be told what the work is.";

/// What a session that carries on the last conversation is told when it was
/// resumed with nothing to say.
const CARRY_ON: &str = "Your last session ended and this one carries on from it. If you were \
                        waiting to be told something, keep waiting; otherwise carry on where you \
                        left off.";

/// What a session is told when the host has restarted and resumed it.
const RESUMED: &str = "The host your session runs on restarted, and this session carries on \
                       the conversation from before it. Whatever was under way as it went -- a \
                       reply, a tool -- was cut off and may not have finished. If you were waiting \
                       to be told something, keep waiting; otherwise check what was cut off, then \
                       carry on where you left off.";

/// However much has piled up, a read stops after this many bytes of the raw
/// events, most of which is Claude Code's own bookkeeping.
const RAW_LIMIT: u64 = 8 << 20;

/// The most lines the service takes for one worker in one report.
const MOST_LINES: usize = 1000;

/// The most tasks the service takes for one worker; more refuses the report.
const MOST_TASKS: u32 = 1000;

/// One step of a worker's conversation, as its transcript says it.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// Its opening message, or a message carried to it; `command` names the
    /// command whose text it is, when it is one.
    Told {
        text: String,
        command: Option<String>,
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
        let (from, text, command_id) = match self {
            Entry::Told { text, command } => ("instruction", text, command),
            Entry::Said { text } => ("agent", text, None),
            Entry::Ran { tool, input } => ("tool", format!("{tool} {input}"), None),
        };
        Line {
            seq,
            at,
            session_id,
            from,
            text,
            command_id,
        }
    }
}

/// How far into a worker's transcripts has been accounted for: which of them
/// (see `transcripts`), the byte after the last whole line read in it, the
/// `seq` of the line that comes next, and as of that line: whether the
/// worker's last turn has ended with nothing said to it since, how many
/// background tasks its session has open, and whether its last turn was begun
/// by a message (None before any turn of the session).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Cursor {
    pub file: usize,
    pub offset: u64,
    pub seq: u64,
    pub waiting: bool,
    pub tasks: u32,
    pub told: Option<bool>,
}

impl Cursor {
    /// On to the start of the next source, a session of its own.
    fn next_source(&mut self) {
        self.file += 1;
        self.offset = 0;
        self.tasks = 0;
        self.told = None;
    }
}

/// The lines read from a worker's transcripts, and where the read after this
/// one starts. A batch with no lines still moves the cursor past what it read:
/// those bytes were read and found to say nothing.
#[derive(Debug)]
pub struct Batch {
    pub lines: Vec<Line>,
    pub next: Cursor,
    /// The last reading of the account's usage among the lines read.
    pub usage: Option<Usage>,
}

/// Where a worker's lines are read from, in the order they were written.
#[derive(Debug, PartialEq)]
enum Source<'a> {
    /// Claude Code's own transcript of a session an older release began, up
    /// to where a headless session carried the conversation on, if
    /// one has: what comes after is in that one's events too.
    Transcript { claude: &'a str, until: Option<u64> },
    /// What a headless session said on its stdout.
    Events { session_id: &'a str },
}

/// A worker's sources, each once: an older release's session that carried on
/// the last conversation wrote on in the transcript before it, and a session resumed
/// after a restart writes on in its own events.
fn sources(sessions: &[Session]) -> Vec<Source<'_>> {
    let mut found = Vec::new();
    for s in sessions {
        let source = if s.stream {
            Source::Events {
                session_id: &s.session_id,
            }
        } else {
            Source::Transcript {
                claude: &s.claude,
                until: sessions
                    .iter()
                    .filter(|o| o.stream && o.claude == s.claude)
                    .map(|o| o.from)
                    .min(),
            }
        };
        if !found.contains(&source) {
            found.push(source);
        }
    }
    found
}

pub struct Workers {
    sessions: Sessions,
    claude: Claude,
    dir: PathBuf,
}

impl Workers {
    pub fn new(sessions: Sessions, claude: Claude, dir: PathBuf) -> Self {
        Workers {
            sessions,
            claude,
            dir,
        }
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

    /// Runs a session for the worker in its folder: a new conversation, or
    /// with `resume` the worker's last one carried on. `text` is what it is
    /// told first.
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
        if let Some(running) = self.live().remove(id) {
            return if running == session_id {
                Ok(())
            } else {
                Err(format!(
                    "{} already has session {running} running: stop it first",
                    record.name
                ))
            };
        }
        if sessions.iter().any(|s| s.session_id == session_id) {
            return Ok(());
        }
        self.claude.installed()?;
        let previous = sessions.last().filter(|_| resume);
        let session = Session {
            session_id: session_id.into(),
            claude: previous.map_or_else(|| claude::uuid(session_id), |p| p.claude.clone()),
            from: previous.map_or(0, |p| transcript_len(&home, &p.claude)),
            stream: true,
            opening: given.command.filter(|_| text.is_some()).map(claude::uuid),
            stopped: false,
            ended: false,
        };
        let begin = match previous {
            Some(_) => Begin::Resume(&session.claude),
            None => Begin::Fresh(&session.claude),
        };
        let text = text.unwrap_or(match begin {
            Begin::Resume(_) => CARRY_ON,
            Begin::Fresh(_) => NOTHING_YET,
        });
        let prompt = home.prompt();
        fs::write(&prompt, text).map_err(|e| format!("{}: {e}", prompt.display()))?;
        // A start without a context keeps the last one the worker was given.
        let written = match given.context {
            Some(c) => Some(context::write(
                &home.claude(),
                c,
                given.documents.unwrap_or_default(),
            )?),
            None => context::written(&home.claude()),
        };
        // The rules follow the host's version, so each session is told the
        // ones of the host it runs on, then the context's.
        let rules = context::rules(&rules(&home, &record), written.as_ref());
        self.claude.prepare(&home.claude(), &home.dir, &rules)?;
        // Its variables go with its tokens, so a session resumed after the
        // host restarts is given them again.
        let mut variables: BTreeMap<String, String> = context::env(given.env).into_iter().collect();
        variables.extend(context::env(given.secrets));
        context::credentials(&home.credentials(), Some(&variables))?;
        // Written before it runs, so a machine that dies between the two
        // leaves a session it knows of, which is then resumed.
        home.begin(&session)?;
        if let Err(e) = self.run(&home, id, &session, begin, text, true) {
            let _ = home.withdraw(session_id);
            return Err(e);
        }
        Ok(())
    }

    /// Runs Claude Code for `session` and tells it `text`: the start's own
    /// message when `opening`, or one the host made up.
    fn run(
        &self,
        home: &Home,
        id: &str,
        session: &Session,
        begin: Begin,
        text: &str,
        opening: bool,
    ) -> Result<(), String> {
        let argv = self.claude.argv(begin, &home.credentials());
        let env = self.claude.env(&home.claude());
        self.sessions.start(
            id,
            &session.session_id,
            Spawn {
                argv: &argv,
                dir: &home.dir,
                env: &env,
                events: &home.events(&session.session_id),
                errors: &home.errors(&session.session_id),
            },
        )?;
        let uuid = match &session.opening {
            Some(uuid) if opening => uuid.clone(),
            _ => claude::host_uuid(&format!("{}/{}", session.session_id, sys::now())),
        };
        if let Err(e) = self.say(id, text, &uuid) {
            let _ = self.sessions.stop(id);
            return Err(e);
        }
        Ok(())
    }

    /// Every live session, by the id of the worker it is for, mapped to its
    /// session id. A session found to have ended by itself is noted as ended,
    /// so the worker reads as interrupted and a host that restarts leaves it.
    pub fn live(&self) -> HashMap<String, String> {
        for (worker, session_id, status) in self.sessions.reap() {
            crate::log!("worker {worker}: session {session_id} ended by itself ({status})");
            if let Ok((home, _)) = self.find(&worker) {
                if home
                    .latest()
                    .is_some_and(|s| s.session_id == session_id && !s.stopped)
                {
                    let _ = home.ended(&session_id);
                }
            }
        }
        self.sessions.live()
    }

    fn running(&self, id: &str) -> Result<(), String> {
        self.live()
            .contains_key(id)
            .then_some(())
            .ok_or_else(|| format!("worker {id} has no live session on this host"))
    }

    /// Takes up every session that was live when the host last went, as a
    /// host does once it has started: each is resumed headless and told the
    /// host restarted, losing the turn that was in flight. A session that
    /// was stopped, or ended by itself, is left: the latter reads as
    /// interrupted.
    pub fn reconcile(&self) -> Vec<(String, State)> {
        let all = self.all();
        let left = |s: &Session| !s.stopped && !s.ended;
        for (home, record) in &all {
            let Some(latest) = home.latest().filter(left) else {
                continue;
            };
            if let Err(e) = self.resume(home, record, &latest) {
                crate::log!(
                    "worker {}: session {} could not be resumed: {e}",
                    record.name,
                    latest.session_id
                );
                let _ = home.ended(&latest.session_id);
            }
        }
        let live = self.live();
        all.into_iter()
            .map(|(home, record)| {
                let (state, _) = self.state(&home, live.contains_key(&record.id), false);
                (record.name, state)
            })
            .collect()
    }

    /// Resumes `latest`, the session the worker had when the host went. One
    /// an older release began is begun again under its own id, headless.
    fn resume(&self, home: &Home, record: &Record, latest: &Session) -> Result<(), String> {
        self.claude.installed()?;
        let session = if latest.stream {
            latest.clone()
        } else {
            let session = Session {
                from: transcript_len(home, &latest.claude),
                stream: true,
                ..latest.clone()
            };
            home.begin(&session)?;
            session
        };
        // A session cut off before Claude Code wrote anything has nothing to
        // resume, and is begun again as it was first.
        let (begin, text, opening) = match claude::transcript(&home.claude(), &session.claude) {
            Some(_) => (Begin::Resume(&session.claude), RESUMED.to_string(), false),
            None => (
                Begin::Fresh(&session.claude),
                fs::read_to_string(home.prompt()).unwrap_or_else(|_| NOTHING_YET.into()),
                true,
            ),
        };
        self.run(home, &record.id, &session, begin, &text, opening)
    }

    /// What the worker is doing. `live` is whether it has a session running,
    /// and `waiting` what its session said of its last turn.
    pub fn state(&self, home: &Home, live: bool, waiting: bool) -> (State, Option<String>) {
        let latest = home.latest();
        if live {
            let state = if waiting {
                State::Waiting
            } else {
                State::Working
            };
            return (state, latest.map(|s| s.session_id));
        }
        match latest {
            Some(s) if !s.stopped => (State::Interrupted, Some(s.session_id)),
            _ => (State::Idle, None),
        }
    }

    /// What is reported of the worker, as of `at` in its transcripts.
    pub fn report(&self, home: &Home, id: &str, live: bool, at: &Cursor) -> WorkerReport {
        let (state, session_id) = self.state(home, live, at.waiting);
        WorkerReport {
            id: id.into(),
            state,
            session_id,
            // A session not running has no tasks, whatever it last said.
            tasks: if live { at.tasks.min(MOST_TASKS) } else { 0 },
            told: at.told,
        }
    }

    /// Writes `text` to the worker's session now, whether it is working or
    /// waiting. Working, it takes the text at its next step rather than when
    /// it has finished; waiting on a question, the text is the answer and the
    /// same session goes on. Claude Code echoes it with `uuid` once it has
    /// taken it.
    pub fn say(&self, id: &str, text: &str, uuid: &str) -> Result<(), String> {
        self.sessions.send(id, &claude::message(text, uuid))
    }

    /// Stops the turn the worker is in, as Escape does for a person at its
    /// terminal: the session and its conversation go on, waiting, and `text`
    /// is what it is told next. `command` names the interrupt.
    pub fn interrupt(&self, id: &str, command: &str, text: Option<&str>) -> Result<(), String> {
        self.running(id)?;
        self.sessions.send(id, &claude::interrupt(command))?;
        match text {
            Some(text) => self.say(id, text, &claude::uuid(command)),
            None => Ok(()),
        }
    }

    /// Ends the worker's session, and Claude Code with it; the worker is idle.
    /// Its folder is left as it is. One with no
    /// session, or not here at all, is already stopped, which is what was
    /// asked; one whose session ended without a stop is idle from now on.
    pub fn stop(&self, id: &str) -> Result<(), String> {
        let Ok((home, _)) = self.find(id) else {
            return Ok(());
        };
        self.sessions.stop(id)?;
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

    /// What the worker has done since `from`, in all its sessions, as much
    /// as fits in `budget` bytes of lines; the rest waits for the next read.
    /// None when nothing new has been written. Only whole lines are read from
    /// the source still being written, so a line in the middle of being
    /// written is left for next time; one an ended session left torn is
    /// passed over.
    pub fn read(&self, home: &Home, from: Cursor, budget: usize) -> Option<Batch> {
        let sessions = home.sessions();
        let sources = sources(&sessions);
        let (mut next, mut lines, mut size) = (from, Vec::new(), 0);
        let mut last_usage = None;
        'files: while next.file < sources.len() {
            let source = &sources[next.file];
            let last = next.file + 1 == sources.len();
            let (path, until) = match source {
                Source::Transcript { claude, until } => {
                    (claude::transcript(&home.claude(), claude), *until)
                }
                Source::Events { session_id } => (Some(home.events(session_id)), None),
            };
            let file = path.and_then(|p| File::open(p).ok());
            let Some(mut file) = file else {
                if last {
                    break;
                }
                next.next_source();
                continue;
            };
            if file.seek(SeekFrom::Start(next.offset)).is_err() {
                break;
            }
            let ranges: Vec<&Session> = match source {
                Source::Transcript { claude, .. } => sessions
                    .iter()
                    .filter(|s| !s.stream && s.claude == *claude)
                    .collect(),
                Source::Events { session_id } => sessions
                    .iter()
                    .filter(|s| s.stream && s.session_id == *session_id)
                    .take(1)
                    .collect(),
            };
            // A transcript a headless session carried on is read only up to
            // where it did: the rest is in that session's events.
            let left = until.map_or(RAW_LIMIT, |u| u.saturating_sub(next.offset));
            let bounded = left <= RAW_LIMIT && until.is_some();
            let mut raw = BufReader::new(file).take(left.min(RAW_LIMIT));
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
                    if last || (taken >= RAW_LIMIT && !bounded) {
                        break 'files;
                    }
                    next.next_source();
                    continue 'files;
                }
                let session = ranges
                    .iter()
                    .rev()
                    .find(|s| s.stream || s.from <= next.offset)
                    .or(ranges.first())
                    .map_or("", |s| s.session_id.as_str());
                // Lossy, so a line that is not UTF-8 is passed over rather
                // than holding the cursor in front of it for ever.
                let claude::Read {
                    at,
                    entries,
                    waiting,
                    tasks,
                    told,
                    usage,
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
                next.tasks = tasks.unwrap_or(next.tasks);
                next.told = told.or(next.told);
                if let Some(windows) = usage {
                    last_usage = Some(Usage {
                        read_at: at,
                        windows,
                    });
                }
                lines.extend(read);
            }
        }
        (next != from).then_some(Batch {
            lines,
            next,
            usage: last_usage,
        })
    }
}

/// What a start carries besides its message, and the start's own id.
#[derive(Debug, Default, Clone, Copy)]
pub struct Given<'a> {
    pub command: Option<&'a str>,
    pub context: Option<&'a Context>,
    pub documents: Option<&'a [Context]>,
    pub env: Option<&'a BTreeMap<String, String>>,
    pub secrets: Option<&'a BTreeMap<String, String>>,
}

/// The worker's standing rules, Claude Code's user memory.
fn rules(home: &Home, record: &Record) -> String {
    format!(
        "You are worker `{}`, and your folder is `{}`. A person reads everything you \
         write, and what to do comes to you as messages. Clone whatever you need into \
         your folder; `home/`, `logs/`, `credentials.env`, `prompt.md`, `sessions.jsonl` \
         and `worker.json` are the host's records of you, so leave them be.\n",
        record.name,
        home.dir.display()
    )
}

/// Messages written to workers' sessions and not yet taken. Each is written
/// to its session's stdin at once, and is taken only once Claude Code echoes
/// it back, which it does at its next step: after it has thought, or once the
/// tool it is in has finished, however long that is. Until then it is only in
/// a pipe. Held in memory, so a host that restarts mid-delivery cannot say
/// what became of a message; its echo still names it (`Entry::Told`).
#[derive(Default)]
pub struct Deliveries {
    held: Vec<Delivery>,
}

struct Delivery {
    command: String,
    worker: String,
    session: String,
    uuid: String,
    echoes: Echoes,
}

impl Deliveries {
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Whether `command` is already being delivered. The service sends a
    /// command again until it is reported, and a message written twice is
    /// said twice.
    pub fn holds(&self, command: &str) -> bool {
        self.held.iter().any(|d| d.command == command)
    }

    /// Writes a message to its worker's session, to be held until it is
    /// taken. Err when it could not be written.
    pub fn hold(
        &mut self,
        workers: &Workers,
        command: &str,
        worker: &str,
        text: &str,
    ) -> Result<(), String> {
        workers.running(worker)?;
        let (session, echoes) = workers
            .sessions
            .echoes(worker)
            .ok_or_else(|| format!("worker {worker} has no live session on this host"))?;
        let uuid = claude::uuid(command);
        workers.say(worker, text, &uuid)?;
        self.held.push(Delivery {
            command: command.into(),
            worker: worker.into(),
            session,
            uuid,
            echoes,
        });
        Ok(())
    }

    /// Hands back, by command, the messages that have been taken or never
    /// will be: their session has ended, whether or not another has begun.
    pub fn settle(&mut self, workers: &Workers) -> Vec<(String, Result<(), String>)> {
        let live = workers.live();
        let mut settled = Vec::new();
        self.held.retain(|d| {
            let result = if d.echoes.contains(&d.uuid) {
                Ok(())
            } else if live.get(&d.worker) != Some(&d.session) {
                Err(format!(
                    "worker {}'s session ended before it took the message",
                    d.worker
                ))
            } else {
                return true;
            };
            settled.push((d.command.clone(), result));
            false
        });
        settled
    }
}

/// How long Claude Code's own transcript of `claude` is now.
fn transcript_len(home: &Home, claude: &str) -> u64 {
    claude::transcript(&home.claude(), claude)
        .and_then(|path| fs::metadata(path).ok())
        .map_or(0, |m| m.len())
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
