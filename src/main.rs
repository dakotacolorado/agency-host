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
use std::{env, path::PathBuf, process::ExitCode, thread::sleep};
use worker::{Cursor, Deliveries, Workers};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// A commit, or `dirty` or `unknown`; see build.rs.
const COMMIT: &str = env!("WORKER_HOST_COMMIT");

/// Bytes of transcript lines one worker may add to a heartbeat. Transcripts
/// are unbounded and bursty, and a heartbeat grown until it fails loses
/// liveness for the sake of logs; what does not fit goes on the next one.
const TRANSCRIPT_PER_WORKER: usize = 32 << 10;

/// Bytes of transcript lines in one report on the socket, for every worker
/// together. API Gateway takes a frame of 32 KB at most, and tungstenite
/// sends a message as one frame.
const TRANSCRIPT_PER_PUSH: usize = 20 << 10;

/// How often the host looks at what its workers are doing, so that a change
/// is on the socket within about this long.
const TICK: Duration = Duration::from_secs(1);

/// How soon to send the next heartbeat while a message is on its way and the
/// socket is down, so it is typed soon after its worker starts listening and
/// reported soon after it arrives.
const DELIVERING: Duration = Duration::from_secs(2);

const USAGE: &str = "usage: worker-host [--config PATH] [--heartbeats N]

  --config PATH   config file (default $XDG_CONFIG_HOME/worker-host/config.toml,
                  or ~/.config/worker-host/config.toml); WORKER_HOST_CONFIG also names it
  --heartbeats N  stop after N heartbeats, failing on the first one that does not land;
                  heartbeats alone, with no socket

Settings, each overriding the file:
  WORKER_HOST_SERVICE_URL  service_url  the worker-service to send heartbeats to
  WORKER_HOST_TOKEN        token
  WORKER_HOST_ID           host_id      defaults to the hostname
  WORKER_HOST_CAPACITY     capacity     how many workers this host will run sessions for
  WORKER_HOST_LABELS       labels       what it can run, comma-separated (a list in the file)
  WORKER_HOST_WORKERS_DIR  workers_dir  where workers' folders are (default ~/workers)
  WORKER_HOST_TMUX_SOCKET  tmux_socket  the tmux server sessions run on (default worker-host)";

struct Args {
    config: Option<PathBuf>,
    heartbeats: Option<u64>,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args {
        config: env::var_os("WORKER_HOST_CONFIG").map(PathBuf::from),
        heartbeats: None,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => parsed.config = Some(args.next().ok_or("--config needs a path")?.into()),
            "--heartbeats" => {
                let n = args.next().ok_or("--heartbeats needs a number")?;
                parsed.heartbeats = Some(
                    n.parse()
                        .map_err(|_| format!("--heartbeats {n}: not a number"))?,
                );
            }
            "--version" => {
                // The commit goes last and stays last: two CI steps read this
                // line with `grep -qx "worker-host .* $GITHUB_SHA"`, so
                // anything added after the commit breaks the check that the
                // binary on a bare machine is the one the workflow just built.
                println!("worker-host {VERSION} {COMMIT}");
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}\n\n{USAGE}")),
        }
    }
    Ok(parsed)
}

fn main() -> ExitCode {
    let args = match parse(env::args().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            log!("{e}");
            return ExitCode::from(2);
        }
    };
    let config = match config::load(args.config, |k| env::var(k).ok()) {
        Ok(config) => config,
        Err(e) => {
            log!("worker-host: {e}");
            return ExitCode::from(2);
        }
    };
    sys::ignore_hangup();
    log!(
        "worker-host {VERSION} {COMMIT}: host {} sending heartbeats to {}",
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
    // A run counted in heartbeats proves the heartbeat alone; one that runs
    // on keeps the socket open beside it.
    let socket = args
        .heartbeats
        .is_none()
        .then(|| Socket::spawn(&config.host_id, &config.token));
    run(
        &Client::new(&config),
        &config,
        &workers,
        socket.as_ref(),
        args.heartbeats,
    )
}

/// Reports not yet heard by the service: those for the heartbeat until one
/// lands, and those for the socket until they are sent on it.
#[derive(Default)]
struct Unheard {
    heartbeat: Vec<CommandReport>,
    socket: Vec<CommandReport>,
}

impl Unheard {
    fn add(&mut self, report: CommandReport) {
        for list in [&mut self.heartbeat, &mut self.socket] {
            if !list.iter().any(|r| r.command_id == report.command_id) {
                list.push(report.clone());
            }
        }
    }
}

fn take(
    command: &Command,
    workers: &Workers,
    deliveries: &mut Deliveries,
    taken: &mut Taken,
    unheard: &mut Unheard,
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
            unheard.add(report);
        }
        None if fresh => log!("command {} ({}): on its way", command.id, command.kind),
        None => {}
    }
}

/// Assumes nothing survives between iterations but what is in memory, the
/// workers' folders and tmux: the machine may sleep for weeks between two,
/// and the next heartbeat is simply late. Reports and transcript that did not
/// land are sent again on the next heartbeat. A host that restarts sends each
/// transcript again from its start; the `seq` on each line says it is the
/// same.
fn run(
    client: &Client,
    config: &Config,
    workers: &Workers,
    socket: Option<&Socket>,
    limit: Option<u64>,
) -> ExitCode {
    let mut unheard = Unheard::default();
    // How far each worker's transcript has been sent in a heartbeat that
    // landed, and how far it has been read to tell what the worker is doing
    // and to send on the socket.
    let mut landed_at: HashMap<String, Cursor> = HashMap::new();
    let mut read_at: HashMap<String, Cursor> = HashMap::new();
    // What the socket was last told of the workers, and on which socket.
    let mut pushed: Option<(u64, Vec<WorkerReport>)> = None;
    let mut deliveries = Deliveries::new(worker::PATIENCE);
    let mut taken = Taken::default();
    let mut waiting: Vec<Command> = Vec::new();
    let mut next_heartbeat = Instant::now();
    let mut landed = 0;
    let mut failures = 0u32;
    loop {
        if let Some(socket) = socket {
            waiting.extend(socket.commands.try_iter());
        }
        for command in waiting.drain(..) {
            take(&command, workers, &mut deliveries, &mut taken, &mut unheard);
        }
        for (command, result) in deliveries.settle(workers) {
            log!(
                "command {command} (message): {}",
                result.as_ref().err().map_or("done", String::as_str)
            );
            let report = CommandReport::new(command, result.err());
            taken.remember(&report);
            unheard.add(report);
        }

        let all = workers.all();
        let live = workers.live();
        let mut fresh: Vec<(String, Vec<Line>)> = Vec::new();
        let mut room = TRANSCRIPT_PER_PUSH;
        for (home, record) in &all {
            let from = read_at.get(&record.id).copied().unwrap_or_default();
            if let Some(batch) = workers.read(home, from, room.max(1)) {
                room = room.saturating_sub(
                    batch
                        .lines
                        .iter()
                        .map(|l| serde_json::to_string(l).map_or(0, |s| s.len()))
                        .sum(),
                );
                read_at.insert(record.id.clone(), batch.next);
                fresh.push((record.id.clone(), batch.lines));
            }
        }
        let states: Vec<WorkerReport> = all
            .iter()
            .map(|(home, record)| {
                let waiting = read_at.get(&record.id).is_some_and(|c| c.waiting);
                let (state, session_id) =
                    workers.state(home, live.get(&record.id).map(String::as_str), waiting);
                WorkerReport {
                    id: record.id.clone(),
                    state,
                    session_id,
                }
            })
            .collect();

        if let Some(socket) = socket.filter(|s| s.is_up()) {
            let changed = pushed
                .as_ref()
                .is_none_or(|(opened, was)| *opened != socket.opened() || *was != states);
            let said = fresh.iter().any(|(_, lines)| !lines.is_empty());
            if (changed || said || !unheard.socket.is_empty())
                && socket.send(&Up::Report {
                    workers: &states,
                    lines: line_reports(&fresh),
                    command_results: &unheard.socket,
                })
            {
                unheard.socket.clear();
                pushed = Some((socket.opened(), states.clone()));
            }
        }

        if Instant::now() >= next_heartbeat {
            let read: Vec<(String, Option<worker::Batch>)> = all
                .iter()
                .map(|(home, record)| {
                    let from = landed_at.get(&record.id).copied().unwrap_or_default();
                    (
                        record.id.clone(),
                        workers.read(home, from, TRANSCRIPT_PER_WORKER),
                    )
                })
                .collect();
            let lines: Vec<(String, Vec<Line>)> = read
                .iter()
                .filter_map(|(id, batch)| Some((id.clone(), batch.as_ref()?.lines.clone())))
                .collect();
            let sent = client.heartbeat(&Heartbeat {
                version: VERSION,
                labels: config.labels.as_deref(),
                capacity: config.capacity,
                workers: &states,
                lines: line_reports(&lines),
                command_results: &unheard.heartbeat,
            });
            next_heartbeat = match sent {
                Ok(reply) => {
                    landed += 1;
                    failures = 0;
                    for (id, batch) in read {
                        if let Some(batch) = batch {
                            landed_at.insert(id, batch.next);
                        }
                    }
                    unheard = Unheard::default();
                    if let Some(socket) = socket {
                        socket.point_at(reply.socket_url.as_deref());
                    }
                    for command in &reply.commands {
                        take(command, workers, &mut deliveries, &mut taken, &mut unheard);
                    }
                    if let Some(n) = limit.filter(|&n| landed >= n) {
                        // One more heartbeat carries the last commands'
                        // reports, which would otherwise never leave.
                        if (unheard.heartbeat.is_empty() && deliveries.is_empty()) || landed > n {
                            return ExitCode::SUCCESS;
                        }
                        Instant::now()
                    } else {
                        let interval = Duration::from_secs_f64(
                            reply.heartbeat_interval.unwrap_or(30.0).clamp(1.0, 300.0),
                        );
                        let hurry = !deliveries.is_empty() && !socket.is_some_and(Socket::is_up);
                        Instant::now()
                            + if hurry {
                                interval.min(DELIVERING)
                            } else {
                                interval
                            }
                    }
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
                    if limit.is_some() {
                        return ExitCode::FAILURE;
                    }
                    Instant::now() + wait
                }
            };
        }

        // Until the next look, or a command pushed before it, which is taken
        // at once.
        let wait = TICK.min(next_heartbeat.saturating_duration_since(Instant::now()));
        match socket {
            Some(socket) => waiting.extend(socket.commands.recv_timeout(wait).ok()),
            None => sleep(wait),
        }
    }
}
