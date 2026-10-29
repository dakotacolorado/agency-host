//! Commands as the host takes them. The service pushes a command on the
//! socket when it is written, and again, with every other still pending,
//! each time the socket opens, until a report naming it is in. So the same
//! command arrives more than once and must be acted on once.

use std::collections::VecDeque;

use crate::protocol::{Command, CommandReport};
use crate::worker::{Deliveries, Given, Workers};

/// How many settled commands are remembered. A command is sent again only
/// until its report is in, which is the next report on an open socket, so
/// this is far more than are ever settled in that time.
const REMEMBERED: usize = 1000;

/// The commands taken and settled, so one sent again is answered as before
/// rather than done twice. Held in memory: a host that restarts takes again
/// a command it had settled but not reported, which is why each kind is also
/// safe to take again on its own (see `Workers`), except a message, which is
/// typed again.
pub struct Taken {
    settled: VecDeque<CommandReport>,
}

impl Default for Taken {
    fn default() -> Self {
        Taken {
            settled: VecDeque::with_capacity(REMEMBERED),
        }
    }
}

impl Taken {
    pub fn knows(&self, command: &str) -> bool {
        self.settled.iter().any(|r| r.command_id == command)
    }

    pub fn remember(&mut self, report: &CommandReport) {
        if self.settled.len() == REMEMBERED {
            self.settled.pop_front();
        }
        self.settled.push_back(report.clone());
    }

    /// Takes `command` unless it was settled already, in which case its
    /// report is handed back again. None while a message is on its way, or
    /// for a command already on its way.
    pub fn take(
        &mut self,
        command: &Command,
        workers: &Workers,
        deliveries: &mut Deliveries,
    ) -> Option<CommandReport> {
        if let Some(report) = self.settled.iter().find(|r| r.command_id == command.id) {
            return Some(report.clone());
        }
        let report = take(command, workers, deliveries)?;
        self.remember(&report);
        Some(report)
    }
}

/// A kind the host does not handle is reported by name rather than dropped,
/// so the sender learns the command went nowhere. A message is reported once
/// it reaches its worker, by `Deliveries::settle`, so none is reported here
/// while it is on its way.
pub fn take(
    command: &Command,
    workers: &Workers,
    deliveries: &mut Deliveries,
) -> Option<CommandReport> {
    if deliveries.holds(&command.id) {
        return None;
    }
    let text = command.text.as_deref();
    let error = match (command.kind.as_str(), command.worker_id.as_deref()) {
        ("create", Some(worker)) => match &command.name {
            Some(name) => workers.create(worker, name).err(),
            None => Some("a create needs a name".into()),
        },
        ("start", Some(worker)) => match &command.session_id {
            Some(session) => {
                let given = Given {
                    context: command.context.as_ref(),
                    env: command.env.as_ref(),
                };
                workers
                    .start_with(
                        worker,
                        session,
                        command.resume.unwrap_or(false),
                        text,
                        given,
                    )
                    .err()
            }
            None => Some("a start needs a sessionId".into()),
        },
        ("message", Some(worker)) => match text {
            Some(text) => deliveries.hold(workers, &command.id, worker, text)?.err(),
            None => Some("a message needs text".into()),
        },
        ("interrupt", Some(worker)) => workers.interrupt(worker, text).err(),
        ("stop", Some(worker)) => workers.stop(worker).err(),
        ("retire", Some(worker)) => workers.retire(worker).err(),
        (kind @ ("create" | "start" | "message" | "interrupt" | "stop" | "retire"), None) => {
            Some(format!("{kind:?} needs a workerId"))
        }
        (kind, _) => Some(format!("this host does not handle {kind:?} commands")),
    };
    Some(CommandReport::new(command.id.clone(), error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::Claude;
    use crate::tmux::Tmux;
    use std::time::Duration;

    /// No tmux server answers on this socket and no folder holds a worker,
    /// so the host has none.
    fn no_workers() -> Workers {
        Workers::new(
            Tmux::new(format!("worker-host-test-{}-none", std::process::id())),
            Claude::new("/nonexistent/claude", None),
            "/nonexistent/workers".into(),
        )
    }

    fn command(json: &str) -> Command {
        serde_json::from_str(json).unwrap()
    }

    fn take_now(json: &str) -> CommandReport {
        take(
            &command(json),
            &no_workers(),
            &mut Deliveries::new(Duration::ZERO),
        )
        .unwrap()
    }

    #[test]
    fn an_unhandled_command_is_reported_not_dropped() {
        let report = take_now(r#"{"id":"c1","kind":"reboot"}"#);
        assert_eq!(
            report,
            CommandReport::new(
                "c1".into(),
                Some(r#"this host does not handle "reboot" commands"#.into())
            )
        );
    }

    #[test]
    fn a_command_for_a_worker_names_one() {
        for kind in ["create", "start", "message", "interrupt", "stop", "retire"] {
            let report = take_now(&format!(r#"{{"id":"c1","kind":"{kind}","text":"hi"}}"#));
            assert_eq!(
                report.error.as_deref(),
                Some(format!("{kind:?} needs a workerId").as_str())
            );
        }
    }

    #[test]
    fn a_command_without_its_fields_is_refused_with_which() {
        for (json, why) in [
            (
                r#"{"id":"c1","kind":"create","workerId":"w1"}"#,
                "needs a name",
            ),
            (
                r#"{"id":"c1","kind":"start","workerId":"w1"}"#,
                "needs a sessionId",
            ),
            (
                r#"{"id":"c1","kind":"message","workerId":"w1"}"#,
                "needs text",
            ),
        ] {
            let report = take_now(json);
            assert!(!report.ok);
            assert!(report.error.as_deref().unwrap().contains(why), "{report:?}");
        }
    }

    #[test]
    fn a_command_for_a_worker_not_here_is_reported() {
        for kind in ["start", "message", "interrupt"] {
            let report = take_now(&format!(
                r#"{{"id":"c1","kind":"{kind}","workerId":"w9","sessionId":"s1","text":"hi"}}"#
            ));
            assert!(!report.ok, "{kind}");
            let error = report.error.unwrap();
            assert!(
                error.contains("no worker w9") || error.contains("w9 has no live session"),
                "{kind}: {error}"
            );
        }
    }

    #[test]
    fn stopping_or_retiring_a_worker_not_here_is_done() {
        for kind in ["stop", "retire"] {
            let report = take_now(&format!(r#"{{"id":"c1","kind":"{kind}","workerId":"w9"}}"#));
            assert_eq!(report, CommandReport::new("c1".into(), None), "{kind}");
        }
    }

    #[test]
    fn a_create_that_cannot_be_done_says_why() {
        let report = take_now(r#"{"id":"c1","kind":"create","workerId":"w1","name":"Ana"}"#);
        let error = report.error.unwrap();
        assert!(error.contains("not a worker name"), "{error}");
    }

    #[test]
    fn a_command_settled_once_is_answered_again_and_not_done_again() {
        let workers = no_workers();
        let mut deliveries = Deliveries::new(Duration::ZERO);
        let mut taken = Taken::default();
        let start = command(r#"{"id":"c1","kind":"start","workerId":"w9","sessionId":"s1"}"#);
        let first = taken.take(&start, &workers, &mut deliveries).unwrap();
        assert!(!first.ok);
        // Were it taken again, a worker made since would now be started.
        taken.settled[0].error = Some("as first reported".into());
        let again = taken.take(&start, &workers, &mut deliveries).unwrap();
        assert_eq!(again.error.as_deref(), Some("as first reported"));
    }
}
