mod claude;
mod commands;
mod config;
mod heartbeat;
mod home;
mod protocol;
mod socket;
mod sys;
mod tmux;
mod worker;

use commands::Taken;
use config::Config;
use heartbeat::{Client, Failure, Heartbeat};
use protocol::{line_reports, Command, CommandReport, Line, Up, WorkerReport};
use socket::Socket;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use std::{env, path::PathBuf, process::ExitCode};
use worker::{Cursor, Deliveries, Workers};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Bytes of transcript lines in one report, for every worker together; what
/// does not fit goes in the next report. A report is one socket message, which
/// API Gateway takes up to 128 KB, and one line may go past this budget, up to
/// `claude::LONGEST_TEXT`, so that a line longer than it is not held up for
/// ever: the two together stay well inside the message.
const TRANSCRIPT_PER_REPORT: usize = 32 << 10;

/// How often the host looks at what its workers are doing, so that a change
/// is on the socket within about this long.
const TICK: Duration = Duration::from_secs(1);

const USAGE: &str = "usage: worker-host [--config PATH]

  --config PATH   config file (default $XDG_CONFIG_HOME/worker-host/config.toml,
                  or ~/.config/worker-host/config.toml); WORKER_HOST_CONFIG also names it

Every setting is in the file: service_url, token, host_id (default the hostname),
capacity, labels, workers_dir (default ~/workers), tmux_socket (default worker-host).";

fn parse(mut args: impl Iterator<Item = String>) -> Result<Option<PathBuf>, String> {
    let mut config = env::var_os("WORKER_HOST_CONFIG").map(PathBuf::from);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config = Some(args.next().ok_or("--config needs a path")?.into()),
            "--version" => {
                println!("worker-host {VERSION}");
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}\n\n{USAGE}")),
        }
    }
    Ok(config)
}

fn main() -> ExitCode {
    let path = match parse(env::args().skip(1)) {
        Ok(path) => path,
        Err(e) => {
            log!("{e}");
            return ExitCode::from(2);
        }
    };
    let config = match config::load(path) {
        Ok(config) => config,
        Err(e) => {
            log!("worker-host: {e}");
            return ExitCode::from(2);
        }
    };
    // Two hosts on one workers folder would each take every command and run it
    // twice, so the second is refused while the first holds the lock.
    let _lock = match hold(&config.workers_dir) {
        Ok(lock) => lock,
        Err(e) => {
            log!("worker-host: {e}");
            return ExitCode::from(3);
        }
    };
    sys::ignore_hangup();
    log!(
        "worker-host {VERSION}: host {} sending heartbeats to {}",
        config.host_id,
        config.service_url
    );
    match config.capacity {
        Some(n) => log!(
            "will run sessions for {n} workers, labelled {}",
            config.labels.as_deref().unwrap_or_default().join(", ")
        ),
        None => log!("no capacity set: the service will put no worker here until one is"),
    }
    log!(
        "workers are in {0}, their sessions on tmux socket {1}: tmux -L {1} ls",
        config.workers_dir.display(),
        config.tmux_socket
    );
    let workers = Workers::new(
        tmux::Tmux::new(&config.tmux_socket),
        claude::Claude::from_env(|k| env::var(k).ok()),
        config.workers_dir.clone(),
    );
    for (name, state) in workers.reconcile() {
        let state = serde_json::to_value(state).unwrap_or_default();
        log!("worker {name}: {}", state.as_str().unwrap_or_default());
    }
    let socket = Socket::spawn(&config.host_id, &config.token);
    run(&Client::new(&config), &config, &workers, &socket)
}

/// The workers folder's lock, held for as long as this host runs; the system
/// lets it go when the process ends, however it ends.
fn hold(workers_dir: &std::path::Path) -> Result<std::fs::File, String> {
    std::fs::create_dir_all(workers_dir).map_err(|e| format!("{}: {e}", workers_dir.display()))?;
    let path = workers_dir.join(".worker-host.lock");
    let file = std::fs::File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(format!(
            "another worker-host is already running on {}: stop it first",
            workers_dir.display()
        )),
        Err(std::fs::TryLockError::Error(e)) => Err(format!("{}: {e}", path.display())),
    }
}

/// Command results not yet written on a socket, each once.
#[derive(Default)]
struct Unsent(Vec<CommandReport>);

impl Unsent {
    fn add(&mut self, report: CommandReport) {
        if !self.0.iter().any(|r| r.command_id == report.command_id) {
            self.0.push(report);
        }
    }
}

fn take(
    command: &Command,
    workers: &Workers,
    deliveries: &mut Deliveries,
    taken: &mut Taken,
    unsent: &mut Unsent,
) {
    let fresh = !deliveries.holds(&command.id) && !taken.knows(&command.id);
    match taken.take(command, workers, deliveries) {
        Some(report) => {
            if fresh {
                log!(
                    "command {} ({}): {}",
                    command.id,
                    command.kind,
                    report.error.as_deref().unwrap_or("done")
                );
            }
            unsent.add(report);
        }
        None if fresh => log!("command {} ({}): on its way", command.id, command.kind),
        None => {}
    }
}

/// Assumes nothing survives between iterations but what is in memory, the
/// workers' folders and tmux: the machine may sleep for weeks between two,
/// and the next heartbeat is simply late. Commands come only on the socket,
/// and reports go only on it: what has not been written on one -- command
/// results, and transcript past each worker's cursor -- is kept until it is,
/// and every socket opened is first sent every worker's state. A host that
/// restarts sends each transcript again from its start; the `seq` on each
/// line says it is the same.
fn run(client: &Client, config: &Config, workers: &Workers, socket: &Socket) -> ExitCode {
    let mut unsent = Unsent::default();
    // How far each worker's transcript has been written on a socket.
    let mut sent_at: HashMap<String, Cursor> = HashMap::new();
    // What the socket was last told of the workers, and on which socket.
    let mut told: Option<(u64, Vec<WorkerReport>)> = None;
    // Which worker's transcript is read first in the next report.
    let mut turn = 0;
    let mut deliveries = Deliveries::new(worker::PATIENCE);
    let mut taken = Taken::default();
    let mut waiting: Vec<Command> = Vec::new();
    let mut next_heartbeat = Instant::now();
    let mut failures = 0u32;
    loop {
        waiting.extend(socket.commands.try_iter());
        for command in waiting.drain(..) {
            take(&command, workers, &mut deliveries, &mut taken, &mut unsent);
        }
        for (command, result) in deliveries.settle(workers) {
            log!(
                "command {command} (message): {}",
                result.as_ref().err().map_or("done", String::as_str)
            );
            let report = CommandReport::new(command, result.err());
            taken.remember(&report);
            unsent.add(report);
        }

        if socket.is_up() {
            report(
                workers,
                socket,
                &mut unsent,
                &mut sent_at,
                &mut told,
                &mut turn,
            );
        }

        if Instant::now() >= next_heartbeat {
            let sent = client.heartbeat(&Heartbeat {
                version: VERSION,
                labels: config.labels.as_deref(),
                capacity: config.capacity,
            });
            next_heartbeat = Instant::now()
                + match sent {
                    Ok(reply) => {
                        failures = 0;
                        socket.point_at(reply.socket_url.as_deref());
                        Duration::from_secs_f64(
                            reply.heartbeat_interval.unwrap_or(30.0).clamp(1.0, 300.0),
                        )
                    }
                    Err(failure) => {
                        let (message, wait) = match failure {
                            Failure::Refused(code, body) => (
                                format!("refused ({code}): {body}"),
                                Duration::from_secs(600),
                            ),
                            Failure::Transient(e) => {
                                failures += 1;
                                (e, Duration::from_secs(2u64.pow(failures.min(6))))
                            }
                        };
                        log!("heartbeat failed: {message}");
                        wait
                    }
                };
        }

        // Until the next look, or a command pushed before it, which is taken
        // at once.
        let wait = TICK.min(next_heartbeat.saturating_duration_since(Instant::now()));
        waiting.extend(socket.commands.recv_timeout(wait).ok());
    }
}

/// Sends what the socket has not yet been told: every worker's state when it
/// is a socket not told before or the states have changed, transcript lines
/// past what was written, and command results not yet written. Nothing is
/// counted as sent until the write is.
///
/// A read takes its first line however long it is, so once the report's
/// budget is spent no more are read: two long lines could not share one
/// message. The workers take turns at being read first, so one with much to
/// say holds none of the others up for long.
fn report(
    workers: &Workers,
    socket: &Socket,
    unsent: &mut Unsent,
    sent_at: &mut HashMap<String, Cursor>,
    told: &mut Option<(u64, Vec<WorkerReport>)>,
    turn: &mut usize,
) {
    let mut all = workers.all();
    let first = *turn % all.len().max(1);
    *turn = turn.wrapping_add(1);
    all.rotate_left(first);
    let live = workers.live();
    let mut read: Vec<(String, Cursor, Vec<Line>)> = Vec::new();
    let mut room = TRANSCRIPT_PER_REPORT;
    for (home, record) in &all {
        let from = sent_at.get(&record.id).copied().unwrap_or_default();
        let batch = (room > 0).then(|| workers.read(home, from, room)).flatten();
        let (next, lines) = match batch {
            Some(batch) => (batch.next, batch.lines),
            None => (from, vec![]),
        };
        room = room.saturating_sub(
            lines
                .iter()
                .map(|l| serde_json::to_string(l).map_or(0, |s| s.len()))
                .sum(),
        );
        read.push((record.id.clone(), next, lines));
    }
    // Back in the order the states were last told in, so that an order is
    // not taken for a change.
    all.rotate_right(first);
    read.rotate_right(first);
    let states: Vec<WorkerReport> = all
        .iter()
        .zip(&read)
        .map(|((home, record), (_, next, _))| {
            let (state, session_id) =
                workers.state(home, live.get(&record.id).map(String::as_str), next.waiting);
            WorkerReport {
                id: record.id.clone(),
                state,
                session_id,
            }
        })
        .collect();
    let opened = socket.opened();
    let changed = told
        .as_ref()
        .is_none_or(|(on, was)| *on != opened || *was != states);
    let said = read.iter().any(|(_, _, lines)| !lines.is_empty());
    if !(changed || said || !unsent.0.is_empty()) {
        // Bytes read that said nothing need no report to be passed.
        for (id, next, _) in read {
            sent_at.insert(id, next);
        }
        return;
    }
    let lines: Vec<(String, Vec<Line>)> = read
        .iter()
        .map(|(id, _, lines)| (id.clone(), lines.clone()))
        .collect();
    if socket.send(&Up::Report {
        workers: &states,
        lines: line_reports(&lines),
        command_results: &unsent.0,
    }) {
        unsent.0.clear();
        for (id, next, _) in read {
            sent_at.insert(id, next);
        }
        *told = Some((opened, states));
    }
}
