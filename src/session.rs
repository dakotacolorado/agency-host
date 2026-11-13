//! Agents' sessions, each a headless Claude Code run as a child of this host:
//! what it is told goes in on its stdin, and what it says comes out on its
//! stdout, which a thread of its own keeps in the session's events file as it
//! comes and answers the permission requests in it. The rest of the host reads
//! that file, never the pipe, so what a session said outlives the host.
//!
//! A session does not outlive the host: its pipes end with it, and on linux it
//! is told to stop when the host goes (`sys::die_with_parent`). The host that
//! starts again resumes each session that was live (`Agents::reconcile`).

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use crate::claude;
use crate::config::Permissions;
use crate::sys;

/// How long a stopped session has to end before it and everything it started
/// is killed.
const GRACE: Duration = Duration::from_secs(5);

pub struct Sessions {
    live: Mutex<HashMap<String, Live>>,
    permissions: Arc<Permissions>,
}

struct Live {
    child: Child,
    session_id: String,
    /// Shared with the thread that answers its requests. None once closed.
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    echoed: Echoes,
}

/// The uuids of the messages one session's Claude Code has echoed back, kept
/// for as long as anyone holds them: an echo read just before the session
/// ended still counts.
#[derive(Clone, Default)]
pub struct Echoes(Arc<Mutex<HashSet<String>>>);

impl Echoes {
    pub fn contains(&self, uuid: &str) -> bool {
        lock(&self.0).contains(uuid)
    }
}

/// How to run one session.
pub struct Spawn<'a> {
    pub argv: &'a [String],
    pub dir: &'a Path,
    pub env: &'a [(String, String)],
    /// Where its stdout's events are kept, and its stderr written.
    pub events: &'a Path,
    pub errors: &'a Path,
}

/// A lock whose holder panicked still guards data that is whole: every
/// change under these locks is one insert, remove or write.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Sessions {
    pub fn new(permissions: Permissions) -> Self {
        Sessions {
            live: Mutex::new(HashMap::new()),
            permissions: Arc::new(permissions),
        }
    }

    /// Runs `spawn` as the session `session_id` for `agent`, appending to
    /// its events and errors, so a session carried on after a restart writes
    /// on where it left off.
    pub fn start(&self, agent: &str, session_id: &str, spawn: Spawn) -> Result<(), String> {
        let append = |path: &Path| {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| format!("{}: {e}", path.display()))
        };
        let mut events = append(spawn.events)?;
        let errors = append(spawn.errors)?;
        // On a line of its own, even after one a machine that died left torn.
        let torn = last_byte(spawn.events).is_some_and(|b| b != b'\n');
        let started = format!(
            "{}{}\n",
            if torn { "\n" } else { "" },
            claude::started(sys::now())
        );
        events
            .write_all(started.as_bytes())
            .map_err(|e| format!("{}: {e}", spawn.events.display()))?;
        let (program, args) = spawn.argv.split_first().ok_or("nothing to run")?;
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(spawn.dir)
            .envs(spawn.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(errors)
            // Its own group, so a stop ends whatever its tools started too,
            // and a signal meant for the host is not its.
            .process_group(0);
        // Safety: between fork and exec, only a system call that allocates
        // nothing is made.
        unsafe {
            command.pre_exec(sys::die_with_parent);
        }
        let mut child = command.spawn().map_err(|e| format!("{program}: {e}"))?;
        let stdin = Arc::new(Mutex::new(child.stdin.take()));
        let echoed = Echoes::default();
        let stdout = child.stdout.take().ok_or("no stdout")?;
        {
            let (stdin, echoed, permissions) =
                (stdin.clone(), echoed.clone(), self.permissions.clone());
            thread::spawn(move || keep(stdout, events, &stdin, &echoed, &permissions));
        }
        lock(&self.live).insert(
            agent.into(),
            Live {
                child,
                session_id: session_id.into(),
                stdin,
                echoed,
            },
        );
        Ok(())
    }

    /// Writes `line` to the agent's session.
    pub fn send(&self, agent: &str, line: &Value) -> Result<(), String> {
        let live = lock(&self.live);
        let session = live
            .get(agent)
            .ok_or_else(|| format!("agent {agent} has no live session on this host"))?;
        write(&session.stdin, line)
    }

    /// The agent's live session, and the echoes of what it takes.
    pub fn echoes(&self, agent: &str) -> Option<(String, Echoes)> {
        lock(&self.live)
            .get(agent)
            .map(|s| (s.session_id.clone(), s.echoed.clone()))
    }

    /// Every agent with a session running, mapped to its session id.
    pub fn live(&self) -> HashMap<String, String> {
        lock(&self.live)
            .iter_mut()
            .filter_map(|(agent, s)| {
                matches!(s.child.try_wait(), Ok(None))
                    .then(|| (agent.clone(), s.session_id.clone()))
            })
            .collect()
    }

    /// The sessions that have ended since last asked, by agent, with their
    /// session ids and how they ended.
    pub fn reap(&self) -> Vec<(String, String, ExitStatus)> {
        let mut live = lock(&self.live);
        let ended: Vec<(String, ExitStatus)> = live
            .iter_mut()
            .filter_map(|(agent, s)| Some((agent.clone(), s.child.try_wait().ok()??)))
            .collect();
        ended
            .into_iter()
            .filter_map(|(agent, status)| {
                let session = live.remove(&agent)?;
                Some((agent, session.session_id, status))
            })
            .collect()
    }

    /// Ends the agent's session and everything it started: its stdin is
    /// closed and its group asked to stop, then killed once `GRACE` is up.
    pub fn stop(&self, agent: &str) -> Result<(), String> {
        let Some(mut session) = lock(&self.live).remove(agent) else {
            return Ok(());
        };
        lock(&session.stdin).take();
        let group = session.child.id();
        sys::signal_group(group, sys::SIGTERM);
        let deadline = Instant::now() + GRACE;
        while matches!(session.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        sys::signal_group(group, sys::SIGKILL);
        session
            .child
            .wait()
            .map(|_| ())
            .map_err(|e| format!("agent {agent}'s session: {e}"))
    }
}

/// Sessions end with the host that runs them, which is also what a host
/// that exits by returning from `main` would do.
impl Drop for Sessions {
    fn drop(&mut self) {
        let agents: Vec<String> = lock(&self.live).keys().cloned().collect();
        for agent in agents {
            let _ = self.stop(&agent);
        }
    }
}

fn last_byte(path: &Path) -> Option<u8> {
    let mut file = File::open(path).ok()?;
    file.seek(SeekFrom::End(-1)).ok()?;
    let mut byte = [0];
    file.read_exact(&mut byte).ok()?;
    Some(byte[0])
}

fn write(stdin: &Mutex<Option<ChildStdin>>, line: &Value) -> Result<(), String> {
    let mut stdin = lock(stdin);
    let pipe = stdin.as_mut().ok_or("the session's stdin is closed")?;
    let mut text = line.to_string();
    text.push('\n');
    pipe.write_all(text.as_bytes())
        .and_then(|()| pipe.flush())
        .map_err(|e| format!("the session's stdin: {e}"))
}

/// Keeps every line of the session's stdout in `events`, each written whole
/// in one append, so a reader finds only whole lines or the last one torn.
/// Answers each request of the host's as it comes, and notes each echo.
fn keep(
    stdout: ChildStdout,
    mut events: File,
    stdin: &Mutex<Option<ChildStdin>>,
    echoed: &Echoes,
    permissions: &Permissions,
) {
    let mut stdout = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        match stdout.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let mut kept = match serde_json::from_slice::<Value>(&line) {
            Ok(event) => {
                if let Some(uuid) = claude::echoed(&event) {
                    lock(&echoed.0).insert(uuid.to_string());
                }
                if let Some(answer) = claude::answer(&event, permissions) {
                    if let Err(e) = write(stdin, &answer) {
                        crate::log!("a session's request went unanswered: {e}");
                    }
                }
                claude::stamped(event, sys::now).to_string().into_bytes()
            }
            Err(_) => line.trim_ascii_end().to_vec(),
        };
        kept.push(b'\n');
        if let Err(e) = events.write_all(&kept) {
            crate::log!("a session's events are not kept: {e}");
        }
    }
}
