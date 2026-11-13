mod agent;
mod claude;
mod commands;
mod config;
mod context;
mod heartbeat;
mod home;
mod protocol;
mod session;
mod socket;
mod sys;

use agent::{Agents, Cursor, Deliveries};
use commands::Taken;
use config::Config;
use heartbeat::{Client, Failure, Heartbeat};
use protocol::{
    line_reports, AgentReport, Command, CommandReport, Line, LineReport, TokenReport, Tokens, Up,
    Usage, MOST_TOKENS,
};
use socket::Socket;
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};
use std::{env, path::PathBuf, process::ExitCode};

/// The release this binary is, as its heartbeat and `--version` say: the tag a
/// release build is given in `AGENT_HOST_RELEASE` (e.g. `v0.1.0-48`), or
/// Cargo.toml's version for any other build.
const VERSION: &str = match option_env!("AGENT_HOST_RELEASE") {
    Some(release) if !release.is_empty() => release,
    _ => env!("CARGO_PKG_VERSION"),
};

/// Bytes of transcript lines in one report, for every agent together; what
/// does not fit goes in the next report. One transcript line may go past it,
/// up to about `claude::LONGEST_TEXT`, so that a line longer than the budget
/// is not held up for ever; `REPORT_LIMIT` is what holds the whole report.
const TRANSCRIPT_PER_REPORT: usize = 32 << 10;

/// The longest report written: API Gateway's 128 KB message, with room to
/// spare. Every report is held to it, whatever the budget let in; see `fit`.
const REPORT_LIMIT: usize = 120 << 10;

/// How often the host looks at what its agents are doing, so that a change
/// is on the socket within about this long.
const TICK: Duration = Duration::from_secs(1);

const USAGE: &str = "usage: agent-host [--config PATH]

  --config PATH   config file (default $XDG_CONFIG_HOME/agent-host/config.toml,
                  or ~/.config/agent-host/config.toml); AGENT_HOST_CONFIG also names it

Every setting is in the file: service_url, token, host_id (default the hostname),
capacity, labels, agents_dir (default ~/agents), [permissions] allow and deny
(default every tool allowed).";

fn parse(mut args: impl Iterator<Item = String>) -> Result<Option<PathBuf>, String> {
    let mut config = env::var_os("AGENT_HOST_CONFIG").map(PathBuf::from);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config = Some(args.next().ok_or("--config needs a path")?.into()),
            "--version" => {
                println!("agent-host {VERSION}");
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
            log!("agent-host: {e}");
            return ExitCode::from(2);
        }
    };
    // Two hosts on one agents folder would each take every command and run it
    // twice, so the second is refused while the first holds the lock.
    let _lock = match hold(&config.agents_dir) {
        Ok(lock) => lock,
        Err(e) => {
            log!("agent-host: {e}");
            return ExitCode::from(3);
        }
    };
    sys::ignore_hangup();
    log!(
        "agent-host {VERSION}: host {} sending heartbeats to {}",
        config.host_id,
        config.service_url
    );
    match config.capacity {
        Some(n) => log!(
            "will run sessions for {n} agents, labelled {}",
            config.labels.as_deref().unwrap_or_default().join(", ")
        ),
        None => log!("no capacity set: the service will put no agent here until one is"),
    }
    log!("agents are in {}", config.agents_dir.display());
    let claude = claude::Claude::from_env(|k| env::var(k).ok(), &config.agents_dir);
    match claude.version() {
        Some(v) if v == claude::CHECKED => log!("Claude Code {v}"),
        Some(v) => log!(
            "Claude Code {v}: its stream-json is checked against {}, and may have changed",
            claude::CHECKED
        ),
        None => match claude.installed() {
            Ok(()) => log!("Claude Code did not say which release it is"),
            Err(e) => log!("{e}: no session can start"),
        },
    }
    let agents = Agents::new(
        session::Sessions::new(config.permissions.clone()),
        claude,
        config.agents_dir.clone(),
    );
    for (name, state) in agents.reconcile() {
        let state = serde_json::to_value(state).unwrap_or_default();
        log!("agent {name}: {}", state.as_str().unwrap_or_default());
    }
    let socket = Socket::spawn(&config.host_id, &config.token);
    run(&Client::new(&config), &config, &agents, &socket)
}

/// The agents folder's lock, held for as long as this host runs; the system
/// lets it go when the process ends, however it ends.
fn hold(agents_dir: &std::path::Path) -> Result<std::fs::File, String> {
    std::fs::create_dir_all(agents_dir).map_err(|e| format!("{}: {e}", agents_dir.display()))?;
    let path = agents_dir.join(".agent-host.lock");
    let file = std::fs::File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(format!(
            "another agent-host is already running on {}: stop it first",
            agents_dir.display()
        )),
        Err(std::fs::TryLockError::Error(e)) => Err(format!("{}: {e}", path.display())),
    }
}

/// Command results not yet written on a socket, each once: a message settled
/// before its report that it was written went up is reported settled alone.
#[derive(Default)]
struct Unsent(Vec<CommandReport>);

/// Each session's tokens as its lines were read, by agent and session, and
/// what the socket was last told of them, and on which socket. A host that
/// restarts reads every session's lines again from the start and comes to the
/// same totals, which it tells again: the service keeps one of each.
#[derive(Default)]
struct Totals {
    read: BySession,
    told: Option<(u64, BySession)>,
}

type BySession = BTreeMap<(String, String), Tokens>;

impl Totals {
    fn read(&mut self, agent: &str, totals: Vec<(String, Tokens)>) {
        for (session, total) in totals {
            self.read.insert((agent.into(), session), total);
        }
    }

    /// Only the agents still here are kept, so a retired agent's sessions
    /// are not told again on every socket.
    fn keep(&mut self, here: impl Fn(&str) -> bool) {
        self.read.retain(|(agent, _), _| here(agent));
    }

    /// The totals `opened` has not been told, every one on a socket not told
    /// before, as many as one report takes.
    fn untold(&self, opened: u64) -> Vec<TokenReport> {
        let told = self.told.as_ref().filter(|(on, _)| *on == opened);
        self.read
            .iter()
            .filter(|(key, total)| told.is_none_or(|(_, was)| was.get(*key) != Some(*total)))
            .take(MOST_TOKENS)
            .map(|((agent, session), total)| TokenReport {
                agent_id: agent.clone(),
                session_id: session.clone(),
                tokens: *total,
            })
            .collect()
    }

    fn sent(&mut self, opened: u64, sent: &[TokenReport]) {
        if self.told.as_ref().is_none_or(|(on, _)| *on != opened) {
            self.told = Some((opened, BTreeMap::new()));
        }
        if let Some((_, told)) = &mut self.told {
            for t in sent {
                told.insert((t.agent_id.clone(), t.session_id.clone()), t.tokens);
            }
        }
    }
}

/// How far each agent's transcript has been written on a socket, and which
/// agent's is read first in the next report.
#[derive(Default)]
struct Reads {
    sent_at: HashMap<String, Cursor>,
    turn: usize,
}

impl Unsent {
    fn add(&mut self, report: CommandReport) {
        match self
            .0
            .iter_mut()
            .find(|r| r.command_id == report.command_id)
        {
            Some(held) if report.settled() => *held = report,
            Some(_) => {}
            None => self.0.push(report),
        }
    }
}

fn take(
    client: &Client,
    command: &Command,
    agents: &Agents,
    deliveries: &mut Deliveries,
    taken: &mut Taken,
    unsent: &mut Unsent,
) {
    let fresh = !deliveries.holds(&command.id) && !taken.knows(&command.id);
    let whole;
    let command = match (&command.context_url, fresh) {
        (Some(path), true) => match client.context(path) {
            Ok(read) => {
                whole = command.with(read);
                &whole
            }
            // What the start carried is still a context, and its session's
            // record says what was left out of it.
            Err(e) => {
                log!(
                    "command {} (start): its whole context could not be read, so it starts with what it carried: {e}",
                    command.id
                );
                command
            }
        },
        _ => command,
    };
    match taken.take(command, agents, deliveries) {
        Some(report) => {
            if fresh {
                let done = if report.settled() {
                    "done"
                } else {
                    "written to its session, not yet taken"
                };
                log!(
                    "command {} ({}): {}",
                    command.id,
                    command.kind,
                    report.error.as_deref().unwrap_or(done)
                );
            }
            unsent.add(report);
        }
        None if fresh => log!("command {} ({}): on its way", command.id, command.kind),
        None => {}
    }
}

/// Assumes nothing survives between iterations but what is in memory, the
/// agents' folders and their sessions: the machine may sleep for weeks between two,
/// and the next heartbeat is simply late. Commands come only on the socket,
/// and reports go only on it: what has not been written on one -- command
/// results, and transcript past each agent's cursor -- is kept until it is,
/// and every socket opened is first sent every agent's state. A host that
/// restarts sends each transcript again from its start; the `seq` on each
/// line says it is the same.
fn run(client: &Client, config: &Config, agents: &Agents, socket: &Socket) -> ExitCode {
    let mut unsent = Unsent::default();
    let mut reads = Reads::default();
    // What the socket was last told of the agents, and on which socket.
    let mut told: Option<(u64, Vec<AgentReport>)> = None;
    // The newest reading of the account's usage not yet written on a socket.
    let mut usage: Option<Usage> = None;
    let mut totals = Totals::default();
    let mut deliveries = Deliveries::default();
    let mut taken = Taken::default();
    let mut waiting: Vec<Command> = Vec::new();
    let mut next_heartbeat = Instant::now();
    let mut failures = 0u32;
    loop {
        waiting.extend(socket.commands.try_iter());
        for command in waiting.drain(..) {
            take(
                client,
                &command,
                agents,
                &mut deliveries,
                &mut taken,
                &mut unsent,
            );
        }
        for (command, result) in deliveries.settle(agents) {
            log!(
                "command {command} (message): {}",
                result.as_ref().err().map_or("taken", String::as_str)
            );
            let report = CommandReport::new(command, result.err());
            taken.remember(&report);
            unsent.add(report);
        }

        if socket.is_up() {
            report(
                agents,
                socket,
                &mut unsent,
                &mut reads,
                &mut told,
                &mut usage,
                &mut totals,
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

/// Sends what the socket has not yet been told: every agent's state when it
/// is a socket not told before or the states have changed, transcript lines
/// past what was written, command results not yet written, the newest
/// reading of the account's usage, if one is, and each session's tokens it was
/// not told. Nothing is counted as sent until the write is.
///
/// A read takes its first line however long it is, so once the report's
/// budget is spent no more are read: two long lines could not share one
/// message. The agents take turns at being read first, so one with much to
/// say holds none of the others up for long.
fn report(
    agents: &Agents,
    socket: &Socket,
    unsent: &mut Unsent,
    reads: &mut Reads,
    told: &mut Option<(u64, Vec<AgentReport>)>,
    usage: &mut Option<Usage>,
    totals: &mut Totals,
) {
    let Reads { sent_at, turn } = reads;
    let mut all = agents.all();
    let first = *turn % all.len().max(1);
    *turn = turn.wrapping_add(1);
    all.rotate_left(first);
    let live = agents.live();
    let mut read: Vec<Read> = Vec::new();
    let mut room = TRANSCRIPT_PER_REPORT;
    for (home, record) in &all {
        let from = sent_at.get(&record.id).copied().unwrap_or_default();
        let batch = (room > 0).then(|| agents.read(home, from, room)).flatten();
        let (next, lines) = match batch {
            Some(batch) => {
                // Every agent shares the account, so only the newest reading
                // matters. A read put back by `fit` is read again later, and
                // a reading sent twice is kept once.
                if let Some(read) = batch.usage {
                    if usage.as_ref().is_none_or(|u| u.read_at <= read.read_at) {
                        *usage = Some(read);
                    }
                }
                // A total put back by `fit` is the same when read again.
                totals.read(&record.id, batch.tokens);
                (batch.next, batch.lines)
            }
            None => (from, vec![]),
        };
        room = room.saturating_sub(lines.iter().map(|l| line_size(&record.id, l)).sum());
        read.push((record.id.clone(), from, next, lines));
    }
    // Back in the order the states were last told in, so that an order is
    // not taken for a change.
    all.rotate_right(first);
    read.rotate_right(first);
    let last_read: Vec<usize> = (0..read.len())
        .rev()
        .map(|i| (i + first) % read.len())
        .collect();
    totals.keep(|agent| all.iter().any(|(_, r)| r.id == agent));
    let opened = socket.opened();
    let tokens = totals.untold(opened);
    let rest = Rest {
        unsent,
        usage: usage.as_ref(),
        tokens: &tokens,
    };
    let states = fit(&mut read, &last_read, &rest, |read| {
        all.iter()
            .zip(read)
            .map(|((home, record), (_, _, next, _))| {
                agents.report(home, &record.id, live.contains_key(&record.id), next)
            })
            .collect()
    });
    let changed = told
        .as_ref()
        .is_none_or(|(on, was)| *on != opened || *was != states);
    let said = read.iter().any(|(.., lines)| !lines.is_empty());
    if !(changed || said || !unsent.0.is_empty() || usage.is_some() || !tokens.is_empty()) {
        // Bytes read that said nothing need no report to be passed.
        for (id, _, next, _) in read {
            sent_at.insert(id, next);
        }
        return;
    }
    let lines: Vec<(String, Vec<Line>)> = read
        .iter()
        .map(|(id, _, _, lines)| (id.clone(), lines.clone()))
        .collect();
    if socket.send(&Up::Report {
        agents: &states,
        lines: line_reports(&lines),
        command_results: &unsent.0,
        usage: usage.as_ref(),
        tokens: &tokens,
    }) {
        unsent.0.clear();
        *usage = None;
        totals.sent(opened, &tokens);
        for (id, _, next, _) in read {
            sent_at.insert(id, next);
        }
        *told = Some((opened, states));
    }
}

/// A line's bytes in a report, where it goes with its agent's id.
fn line_size(agent_id: &str, line: &Line) -> usize {
    serde_json::to_string(&LineReport { agent_id, line }).map_or(0, |s| s.len())
}

/// Each agent's id, where its read began and ended, and what it read.
type Read = (String, Cursor, Cursor, Vec<Line>);

/// What a report carries besides the agents' states and lines.
struct Rest<'a> {
    unsent: &'a Unsent,
    usage: Option<&'a Usage>,
    tokens: &'a [TokenReport],
}

/// The agents' states for a report of `read`, once it is short enough to
/// send. A report over API Gateway's limit closes the socket only once it has
/// been written, so it would be counted sent and lost with all it held: the
/// lines read last (`last_read`, indices into `read`) are put back, their
/// agents' reads undone to go in a later report, until it fits. The states
/// are worked out again each time, since they follow what was read.
fn fit(
    read: &mut [Read],
    last_read: &[usize],
    rest: &Rest,
    states_of: impl Fn(&[Read]) -> Vec<AgentReport>,
) -> Vec<AgentReport> {
    let mut states = states_of(read);
    for &i in last_read {
        if report_size(&states, read, rest) <= REPORT_LIMIT {
            break;
        }
        let (id, from, next, lines) = &mut read[i];
        if lines.is_empty() {
            continue;
        }
        log!(
            "report too long for the socket: {id}'s {} lines wait",
            lines.len()
        );
        (*next, *lines) = (*from, vec![]);
        states = states_of(read);
    }
    states
}

/// A report's bytes as it would be written.
fn report_size(states: &[AgentReport], read: &[Read], rest: &Rest) -> usize {
    let lines: Vec<(String, Vec<Line>)> = read
        .iter()
        .map(|(id, _, _, lines)| (id.clone(), lines.clone()))
        .collect();
    let up = Up::Report {
        agents: states,
        lines: line_reports(&lines),
        command_results: &rest.unsent.0,
        usage: rest.usage,
        tokens: rest.tokens,
    };
    serde_json::to_string(&up).map_or(usize::MAX, |s| s.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(seq: u64, text: &str) -> Line {
        Line {
            seq,
            at: "2026-10-28T00:00:00.000Z".into(),
            session_id: "s1".into(),
            from: "agent",
            text: text.into(),
            command_id: None,
        }
    }

    #[test]
    fn a_message_settled_before_its_written_report_went_up_goes_up_settled_alone() {
        let mut unsent = Unsent::default();
        unsent.add(CommandReport::written("c1".into()));
        unsent.add(CommandReport::new("c1".into(), None));
        unsent.add(CommandReport::written("c1".into()));
        assert_eq!(unsent.0, [CommandReport::new("c1".into(), None)]);
    }

    #[test]
    fn a_report_too_long_for_the_socket_leaves_the_lines_read_last_for_later() {
        let at = |offset| Cursor {
            offset,
            ..Cursor::default()
        };
        let long = "x".repeat(60 << 10);
        let mut read: Vec<Read> = vec![
            ("w1".into(), at(0), at(10), vec![line(0, &long)]),
            ("w2".into(), at(0), at(20), vec![line(0, "short")]),
            ("w3".into(), at(5), at(30), vec![line(0, &long)]),
        ];
        let unsent = Unsent(vec![CommandReport::new("c1".into(), None)]);
        let rest = Rest {
            unsent: &unsent,
            usage: None,
            tokens: &[],
        };
        let states_of = |read: &[Read]| -> Vec<AgentReport> {
            read.iter()
                .map(|(id, _, next, _)| {
                    let state = if next.offset > 0 {
                        protocol::State::Working
                    } else {
                        protocol::State::Idle
                    };
                    AgentReport::new(id.clone(), state, None)
                })
                .collect()
        };
        // w3 was read first, then w1 and w2: w2 and then w1 are put back.
        let states = fit(&mut read, &[1, 0, 2], &rest, states_of);
        assert!(report_size(&states, &read, &rest) <= REPORT_LIMIT);
        let kept: Vec<_> = read
            .iter()
            .map(|(id, _, next, lines)| (id.as_str(), next.offset, lines.len()))
            .collect();
        assert_eq!(kept, [("w1", 0, 0), ("w2", 0, 0), ("w3", 30, 1)]);
        assert_eq!(states, states_of(&read), "states follow what is kept");

        let mut fits = read.clone();
        fit(&mut fits, &[1, 0, 2], &rest, states_of);
        assert_eq!(fits, read, "a report that fits is left as it is");
    }

    #[test]
    fn a_sessions_total_is_told_once_it_changes_and_every_total_on_a_new_socket() {
        let used = |n: u64| Tokens {
            input_tokens: n,
            output_tokens: 10 * n,
            cache_creation_input_tokens: 100 * n,
            cache_read_input_tokens: 1000 * n,
        };
        let told = |reports: &[TokenReport]| -> Vec<(String, u64)> {
            reports
                .iter()
                .map(|t| (t.session_id.clone(), t.tokens.input_tokens))
                .collect()
        };
        let mut totals = Totals::default();
        totals.read("w1", vec![("s1".into(), used(1)), ("s2".into(), used(2))]);
        totals.read("w2", vec![("s3".into(), used(3))]);
        let first = totals.untold(1);
        assert_eq!(
            told(&first),
            [("s1".into(), 1), ("s2".into(), 2), ("s3".into(), 3)]
        );
        assert_eq!(told(&totals.untold(1)), told(&first), "until it is sent");
        totals.sent(1, &first);
        assert_eq!(totals.untold(1), []);

        totals.read("w1", vec![("s2".into(), used(5))]);
        totals.read("w2", vec![("s3".into(), used(3))]);
        let changed = totals.untold(1);
        assert_eq!(told(&changed), [("s2".into(), 5)], "only what changed");
        totals.sent(1, &changed);
        assert_eq!(
            told(&totals.untold(2)),
            [("s1".into(), 1), ("s2".into(), 5), ("s3".into(), 3)],
            "a socket opened again is told every total"
        );

        totals.keep(|agent| agent == "w2");
        assert_eq!(told(&totals.untold(3)), [("s3".into(), 3)]);
        for n in 0..MOST_TOKENS as u64 + 10 {
            totals.read("w2", vec![(format!("t{n:03}"), used(n))]);
        }
        let most = totals.untold(3);
        assert_eq!(most.len(), MOST_TOKENS, "as many as one report takes");
        totals.sent(3, &most);
        assert_eq!(totals.untold(3).len(), 11, "and the rest in the next");
    }
}
