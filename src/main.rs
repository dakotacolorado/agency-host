mod agent;
mod beat;
mod claude;
mod config;
mod machine;
mod sys;
mod tmux;

use agent::Agents;
use beat::{Beat, Client, Failure, Present, Report, Session, CONTRACT};
use config::Config;
use machine::{Meter, Sample};
use std::collections::HashMap;
use std::{env, path::PathBuf, process::ExitCode, thread::sleep, time::Duration};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// A commit, or `dirty` or `unknown`; see build.rs.
const COMMIT: &str = env!("AGENT_HOST_COMMIT");

/// Bytes of transcript lines one agent may add to a beat. Transcripts are
/// unbounded and bursty, and a beat grown until it fails loses liveness for
/// the sake of logs; what does not fit goes on the next beat.
const TRANSCRIPT_PER_AGENT: usize = 32 << 10;

/// Samples kept while beats are failing, the most a beat may carry. A host
/// cut off for longer loses its oldest, which a chart of load can spare.
const SAMPLES_KEPT: usize = 1000;

const USAGE: &str = "usage: agent-host [--config PATH] [--beats N]

  --config PATH  config file (default $XDG_CONFIG_HOME/agent-host/config.toml,
                 or ~/.config/agent-host/config.toml); AGENT_HOST_CONFIG also names it
  --beats N      stop after N beats, failing on the first one that does not land

Settings, each overriding the file:
  AGENT_HOST_SERVICE_URL  service_url  the agent-service to beat to
  AGENT_HOST_TOKEN        token
  AGENT_HOST_ID           host_id      defaults to the hostname
  AGENT_HOST_CAPACITY     capacity     how many agents this host will run; unset, none are placed here
  AGENT_HOST_LABELS       labels       what it can run, comma-separated (a list in the file)";

struct Args {
    config: Option<PathBuf>,
    beats: Option<u64>,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args {
        config: env::var_os("AGENT_HOST_CONFIG").map(PathBuf::from),
        beats: None,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => parsed.config = Some(args.next().ok_or("--config needs a path")?.into()),
            "--beats" => {
                let n = args.next().ok_or("--beats needs a number")?;
                parsed.beats = Some(
                    n.parse()
                        .map_err(|_| format!("--beats {n}: not a number"))?,
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
        "agent-host {VERSION} {COMMIT} (contract {CONTRACT}): host {} beating to {}",
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
    let mut meter = Meter::new(home.clone());
    let agents = Agents::new(
        tmux::Tmux::new("agent-host"),
        claude::Claude::from_env(|k| env::var(k).ok()),
        home,
    );
    run(
        &Client::new(&config),
        &config,
        &agents,
        &mut meter,
        args.beats,
    )
}

/// Assumes nothing survives between beats but what is in memory: the machine
/// may sleep for weeks between two iterations, and the next beat is simply
/// late. Reports and transcript that did not land are sent again on the next
/// beat. A host that restarts sends each transcript again from its start; the
/// `seq` on each line says it is the same.
fn run(
    client: &Client,
    config: &Config,
    agents: &Agents,
    meter: &mut Meter,
    limit: Option<u64>,
) -> ExitCode {
    let persons = tmux::Tmux::persons();
    let mut reports: Vec<Report> = Vec::new();
    let mut samples: Vec<Sample> = Vec::new();
    let mut cursors: HashMap<String, agent::Cursor> = HashMap::new();
    let mut landed = 0;
    let mut failures = 0u32;
    loop {
        let read: Vec<_> = agents
            .ids()
            .into_iter()
            .map(|id| {
                let from = cursors.get(&id).copied().unwrap_or_default();
                let batch = agents.read(&id, from, TRANSCRIPT_PER_AGENT);
                (id, batch)
            })
            .collect();
        if let Some(sample) = meter.sample() {
            if samples.len() == SAMPLES_KEPT {
                samples.remove(0);
            }
            samples.push(sample);
        }
        let wait = match client.beat(&Beat {
            host_version: VERSION,
            host_commit: COMMIT,
            agents: read
                .iter()
                .map(|(id, batch)| {
                    Present::running(id.clone(), batch.as_ref().map_or(&[], |b| &b.lines))
                })
                .collect(),
            reports: &reports,
            capacity: config.capacity,
            labels: config.labels.as_deref(),
            sessions: persons
                .names()
                .map(|names| names.into_iter().map(|name| Session { name }).collect()),
            samples: &samples,
        }) {
            Ok(reply) => {
                landed += 1;
                failures = 0;
                samples.clear();
                for (id, batch) in &read {
                    if let Some(batch) = batch {
                        cursors.insert(id.clone(), batch.next);
                    }
                }
                reports = reply
                    .commands
                    .iter()
                    .map(|command| beat::take(command, agents))
                    .collect();
                for (command, report) in reply.commands.iter().zip(&reports) {
                    log!(
                        "command {} ({}): {}",
                        command.id,
                        command.kind,
                        report.error.as_deref().unwrap_or("done")
                    );
                }
                if let Some(n) = limit.filter(|&n| landed >= n) {
                    // One more beat carries the last commands' reports, which
                    // would otherwise never leave.
                    if reports.is_empty() || landed > n {
                        return ExitCode::SUCCESS;
                    }
                    Duration::ZERO
                } else {
                    Duration::from_secs_f64(reply.come_back_in.unwrap_or(30.0).clamp(1.0, 300.0))
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
                log!("beat failed: {message}");
                if limit.is_some() {
                    return ExitCode::FAILURE;
                }
                wait
            }
        };
        sleep(wait);
    }
}
