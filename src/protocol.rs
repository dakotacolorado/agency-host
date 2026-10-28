//! What worker-service and a host say to each other, the same whether it
//! goes by heartbeat or by socket: commands down, reports up.

use serde::{Deserialize, Serialize};

/// A command for one worker. Every kind names it; the rest is each kind's
/// own. Unknown fields are ignored, so the service can add one before the
/// host reads it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Command {
    pub id: String,
    pub kind: String,
    pub worker_id: Option<String>,
    /// `create`: who the worker is, and the repositories it works in.
    pub name: Option<String>,
    pub role: Option<String>,
    pub repositories: Option<Vec<String>>,
    /// `create`: the worker's own bearer token for the service. The same one
    /// comes on every redelivery, and the first is the one kept.
    pub credential: Option<Credential>,
    /// `start`: the session the service has opened, and whether it carries
    /// on the worker's last conversation.
    pub session_id: Option<String>,
    pub resume: Option<bool>,
    /// `start`, `message`, `interrupt`: what to say to the worker. Who sent
    /// it is the service's record, not the worker's: a person, a worker, and
    /// an answer to a question are all carried the same way.
    pub text: Option<String>,
}

/// A worker's bearer token for the service, as the service issued it on its
/// `create`. It has no `Display`, and its `Debug` names it without showing
/// it, so no log line or error message can carry it by accident.
#[derive(Deserialize)]
#[serde(transparent)]
pub struct Credential(String);

impl Credential {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credential(..)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Has a home, no live session.
    Idle,
    /// A live session, at work.
    Working,
    /// A live session whose turn ended with nothing said to it since: it is
    /// waiting on someone, whether on a question or with its work done,
    /// which its transcript does not tell apart, so the host does not guess.
    Waiting,
    /// A live session held on a question Claude Code is asking at its
    /// terminal: a permission, a tool's choice, a dialog of its own.
    Blocked,
    /// Its last session ended without a stop: a crash, the machine going
    /// down. It can be resumed.
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerReport {
    pub id: String,
    pub state: State,
    /// The live session, or for `interrupted` the session that ended.
    pub session_id: Option<String>,
}

/// One step of a worker's conversation. `seq` counts every line before it in
/// all the worker's sessions, so a line read again -- a report that did not
/// land, or a host that restarted -- has the same `seq` and is kept once.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Line {
    pub seq: u64,
    pub at: String,
    pub session_id: String,
    pub from: &'static str,
    pub text: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LineReport<'a> {
    pub worker_id: &'a str,
    #[serde(flatten)]
    pub line: &'a Line,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandReport {
    pub command_id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl CommandReport {
    pub fn new(command_id: String, error: Option<String>) -> Self {
        CommandReport {
            command_id,
            ok: error.is_none(),
            error,
        }
    }
}

/// The lines read for each worker, by worker id.
pub type Lines = [(String, Vec<Line>)];

pub fn line_reports(lines: &Lines) -> Vec<LineReport<'_>> {
    lines
        .iter()
        .flat_map(|(worker_id, lines)| lines.iter().map(move |line| LineReport { worker_id, line }))
        .collect()
}

/// What the socket carries up: a report, as the heartbeat's fields, or a ping
/// that keeps API Gateway from closing a socket it thinks idle.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Up<'a> {
    #[serde(rename_all = "camelCase")]
    Report {
        workers: &'a [WorkerReport],
        lines: Vec<LineReport<'a>>,
        command_results: &'a [CommandReport],
    },
    Ping,
}

/// What the socket brings down. Only commands are for a host; anything else
/// is passed over, so the service can add a kind before the host reads it.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Down {
    Command {
        command: Box<Command>,
    },
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn each_command_kind_is_read_with_its_fields() {
        let create: Command = serde_json::from_value(json!({
            "id":"c1","kind":"create","workerId":"w1","name":"ana","role":"frontend",
            "repositories":["workplace-ui"],"credential":"wkr_w1_secret","later":"ignored"
        }))
        .unwrap();
        assert_eq!(create.name.as_deref(), Some("ana"));
        assert_eq!(create.repositories, Some(vec!["workplace-ui".into()]));
        assert_eq!(
            create.credential.as_ref().unwrap().expose(),
            "wkr_w1_secret"
        );
        assert!(!format!("{create:?}").contains("secret"), "{create:?}");

        let start: Command = serde_json::from_value(json!({
            "id":"c2","kind":"start","workerId":"w1","sessionId":"s1","resume":true,"text":"go"
        }))
        .unwrap();
        assert_eq!(
            (
                start.session_id.as_deref(),
                start.resume,
                start.text.as_deref()
            ),
            (Some("s1"), Some(true), Some("go"))
        );
    }

    #[test]
    fn a_socket_message_is_a_command_or_passed_over() {
        let down: Down = serde_json::from_str(
            r#"{"type":"command","command":{"id":"c1","kind":"stop","workerId":"w1"}}"#,
        )
        .unwrap();
        let Down::Command { command } = down else {
            panic!("{down:?}")
        };
        assert_eq!((command.id.as_str(), command.kind.as_str()), ("c1", "stop"));
        for other in [r#"{"type":"pong"}"#, r#"{"type":"worker","worker":{}}"#] {
            assert!(matches!(
                serde_json::from_str::<Down>(other).unwrap(),
                Down::Other
            ));
        }
        assert!(serde_json::from_str::<Down>("not json").is_err());
    }

    #[test]
    fn a_report_is_the_heartbeats_fields_under_its_type() {
        let workers = [
            WorkerReport {
                id: "w1".into(),
                state: State::Working,
                session_id: Some("s1".into()),
            },
            WorkerReport {
                id: "w2".into(),
                state: State::Idle,
                session_id: None,
            },
        ];
        let lines = [(
            "w1".to_string(),
            vec![Line {
                seq: 4,
                at: "2026-10-28T00:00:00.000Z".into(),
                session_id: "s1".into(),
                from: "agent",
                text: "hi".into(),
            }],
        )];
        let results = [CommandReport::new("c1".into(), Some("no".into()))];
        let up = Up::Report {
            workers: &workers,
            lines: line_reports(&lines),
            command_results: &results,
        };
        assert_eq!(
            serde_json::to_value(&up).unwrap(),
            json!({
                "type":"report",
                "workers":[
                    {"id":"w1","state":"working","sessionId":"s1"},
                    {"id":"w2","state":"idle","sessionId":null}
                ],
                "lines":[{"workerId":"w1","seq":4,"at":"2026-10-28T00:00:00.000Z","sessionId":"s1","from":"agent","text":"hi"}],
                "commandResults":[{"commandId":"c1","ok":false,"error":"no"}]
            })
        );
        assert_eq!(
            serde_json::to_value(Up::Ping).unwrap(),
            json!({"type":"ping"})
        );
    }
}
