mod agent;
mod claude;
mod config;
mod heartbeat;
mod machine;
mod sys;
mod tmux;
mod workspace;

use agent::{Agents, Deliveries};
use config::Config;
use heartbeat::{Client, Failure, Heartbeat, Present, Report, CONTRACT};
use machine::{Meter, Sample};
use std::collections::HashMap;
use std::{env, path::PathBuf, process::ExitCode, thread::sleep, time::Duration};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// A commit, or `dirty` or `unknown`; see build.rs.
const COMMIT: &str = env!("AGENT_HOST_COMMIT");

/// Bytes of transcript lines one agent may add to a heartbeat. Transcripts are
/// unbounded and bursty, and a heartbeat grown until it fails loses liveness
/// for the sake of logs; what does not fit goes on the next heartbeat.
const TRANSCRIPT_PER_AGENT: usize = 32 << 10;

/// How soon to send the next heartbeat while a message is on its way, so it is
/// typed soon after its agent starts listening and reported soon after it
/// arrives.
const DELIVERING: Duration = Duration::from_secs(2);

/// Samples kept while heartbeats are failing, the most a heartbeat may carry. A
/// host cut off for longer loses its oldest, which a chart of load can spare.
const SAMPLES_KEPT: usize = 1000;

const USAGE: &str = "usage: agent-host [--config PATH] [--heartbeats N]

  --config PATH   config file (default $XDG_CONFIG_HOME/agent-host/config.toml,
                  or ~/.config/agent-host/config.toml); AGENT_HOST_CONFIG also names it
  --heartbeats N  stop after N heartbeats, failing on the first one that does not land

Settings, each overriding the file:
  AGENT_HOST_SERVICE_URL  service_url  the agent-service to send heartbeats to
  AGENT_HOST_TOKEN        token
  AGENT_HOST_ID           host_id      defaults to the hostname
  AGENT_HOST_CAPACITY     capacity     how many agents this host will run; unset, none are placed here
  AGENT_HOST_LABELS       labels       what it can run, comma-separated (a list in the file)
  AGENT_HOST_REPOSITORIES repositories the checkouts an agent can be given a worktree of,
                                       as name=/path,... (a table in the file)";

struct Args {
    config: Option<PathBuf>,
    heartbeats: Option<u64>,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args {
        config: env::var_os("AGENT_HOST_CONFIG").map(PathBuf::from),
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
                // line with `grep -qx "agent-host .* $GITHUB_SHA"`, so anything
                // added after the commit breaks the check that the binary on a
                // bare machine is the one the workflow just built.
                println!("agent-host {VERSION} (contract {CONTRACT}) {COMMIT}");
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
            log!("agent-host: {e}");
            return ExitCode::from(2);
        }
    };
    sys::ignore_hangup();
    log!(
        "agent-host {VERSION} {COMMIT} (contract {CONTRACT}): host {} sending heartbeats to {}",
        config.host_id,
        config.service_url
    );
    match config.capacity {
        Some(n) => log!(
            "will run {n} agents, labelled {}",
            config.labels.as_deref().unwrap_or_default().join(", ")
        ),
        None => log!("no capacity set: the service will place no agent here until one is"),
    }
    let Some(home) = config::agents_dir() else {
        log!("agent-host: no HOME or XDG_DATA_HOME to keep agents' folders in");
        return ExitCode::from(2);
    };
    log!(
        "agents run on tmux socket {0}: tmux -L {0} ls",
        config.tmux_socket
    );
    let mut meter = Meter::new(home.clone());
    let agents = Agents::new(
        tmux::Tmux::new(&config.tmux_socket),
        claude::Claude::from_env(|k| env::var(k).ok()),
        home,
        config.repositories.clone(),
        config.service_url.clone(),
    );
    run(
        &Client::new(&config),
        &config,
        &agents,
        &mut meter,
        args.heartbeats,
    )
}

/// Assumes nothing survives between heartbeats but what is in memory: the
/// machine may sleep for weeks between two iterations, and the next heartbeat
/// is simply late. Reports and transcript that did not land are sent again on
/// the next heartbeat. A host that restarts sends each transcript again from
/// its start; the `seq` on each line says it is the same.
fn run(
    client: &Client,
    config: &Config,
    agents: &Agents,
    meter: &mut Meter,
    limit: Option<u64>,
) -> ExitCode {
    let mut reports: Vec<Report> = Vec::new();
    let mut samples: Vec<Sample> = Vec::new();
    let mut cursors: HashMap<String, agent::Cursor> = HashMap::new();
    let mut deliveries = Deliveries::new(agent::PATIENCE);
    let mut landed = 0;
    let mut failures = 0u32;
    loop {
        for (command, result) in deliveries.settle(agents) {
            log!(
                "command {command} (message): {}",
                result.as_ref().err().map_or("done", String::as_str)
            );
            reports.push(Report::new(command, result.err()));
        }
        let read: Vec<_> = agents
            .ids()
            .into_iter()
            .map(|id| {
                let from = cursors.get(&id).copied().unwrap_or_default();
                let batch = agents.read(&id, from, TRANSCRIPT_PER_AGENT);
                (id, from, batch)
            })
            .collect();
        if let Some(sample) = meter.sample() {
            if samples.len() == SAMPLES_KEPT {
                samples.remove(0);
            }
            samples.push(sample);
        }
        let wait = match client.heartbeat(&Heartbeat {
            version: VERSION,
            agents: read
                .iter()
                .map(|(id, from, batch)| match batch {
                    Some(batch) => Present::running(id.clone(), batch.next.waiting, &batch.lines),
                    None => Present::running(id.clone(), from.waiting, &[]),
                })
                .collect(),
            command_results: &reports,
            capacity: config.capacity,
            labels: config.labels.as_deref(),
            samples: &samples,
        }) {
            Ok(reply) => {
                landed += 1;
                failures = 0;
                samples.clear();
                for (id, _, batch) in &read {
                    if let Some(batch) = batch {
                        cursors.insert(id.clone(), batch.next);
                    }
                }
                reports.clear();
                for command in &reply.commands {
                    let held = deliveries.holds(&command.id);
                    match heartbeat::take(command, agents, &mut deliveries) {
                        Some(report) => {
                            log!(
                                "command {} ({}): {}",
                                command.id,
                                command.kind,
                                report.error.as_deref().unwrap_or("done")
                            );
                            reports.push(report);
                        }
                        None if !held => {
                            log!("command {} ({}): on its way", command.id, command.kind)
                        }
                        None => {}
                    }
                }
                if let Some(n) = limit.filter(|&n| landed >= n) {
                    // One more heartbeat carries the last commands' reports,
                    // which would otherwise never leave.
                    if (reports.is_empty() && deliveries.is_empty()) || landed > n {
                        return ExitCode::SUCCESS;
                    }
                    Duration::ZERO
                } else {
                    let wait = Duration::from_secs_f64(
                        reply.heartbeat_interval.unwrap_or(30.0).clamp(1.0, 300.0),
                    );
                    if deliveries.is_empty() {
                        wait
                    } else {
                        wait.min(DELIVERING)
                    }
                }
            }
            Err(failure) => {
                let (message, wait) = match failure {
                    Failure::Unsupported { minimum, error } => (
                        format!(
                            "contract {CONTRACT} is unsupported, the service accepts {minimum} and up: {error}"
                        ),
                        Duration::from_secs(600),
                    ),
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
                wait
            }
        };
        sleep(wait);
    }
}
