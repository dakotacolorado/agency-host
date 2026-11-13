//! What agent-service and a host say to each other on the socket: commands
//! down, reports up.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::context::Context;

/// A command for one agent. Every kind names it; the rest is each kind's
/// own. Unknown fields are ignored, so the service can add one before the
/// host reads it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Command {
    pub id: String,
    pub kind: String,
    pub agent_id: Option<String>,
    /// `create`: what the agent is called.
    pub name: Option<String>,
    /// `start`: the session the service has opened, and whether it carries
    /// on the agent's last conversation.
    pub session_id: Option<String>,
    pub resume: Option<bool>,
    /// `start`, `message`, `interrupt`: what to say to the agent.
    pub text: Option<String>,
    /// `start`: files to write into the agent's Claude Code folder first,
    /// and variables to set in its session.
    pub context: Option<Context>,
    pub documents: Option<Vec<Context>>,
    /// `start`: where on the service to read the whole context when the
    /// start was too large to carry it; `context` and `documents` then hold
    /// only what fitted.
    pub context_url: Option<String>,
    pub env: Option<BTreeMap<String, String>>,
    /// `start`: tokens for its session, which go only into a file only the
    /// host's user can read, never onto a command line.
    pub secrets: Option<BTreeMap<String, String>>,
}

/// A start's whole context, as the service answers at its `contextUrl`.
#[derive(Debug, Deserialize)]
pub struct Whole {
    pub context: Option<Context>,
    pub documents: Vec<Context>,
}

impl Command {
    /// This start with `whole` in place of the context it carried.
    pub fn with(&self, whole: Whole) -> Command {
        Command {
            context: whole.context,
            documents: Some(whole.documents),
            context_url: None,
            ..self.clone()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Has a home, no live session.
    Idle,
    /// A live session, at work.
    Working,
    /// A live session whose turn ended with nothing said to it since. Whether
    /// that is on a question, with its work done or on its own subagents its
    /// transcript does not tell apart, so the host does not guess: the agent
    /// tells the service itself.
    Waiting,
    /// Its last session ended without a stop: a crash, the machine going
    /// down. It can be resumed.
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentReport {
    pub id: String,
    pub state: State,
    /// The live session, or for `interrupted` the session that ended.
    pub session_id: Option<String>,
    /// The background tasks and subagents its live session has open, whether
    /// or not it is in a turn.
    pub tasks: u32,
    /// Whether its turn, or its last, was begun by a message the host
    /// delivered rather than by itself. Not said before its session's first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub told: Option<bool>,
}

impl AgentReport {
    #[cfg(test)]
    pub fn new(id: String, state: State, session_id: Option<String>) -> Self {
        AgentReport {
            id,
            state,
            session_id,
            tasks: 0,
            told: None,
        }
    }
}

/// One step of an agent's conversation. `seq` counts every line before it in
/// all the agent's sessions, so a line read again -- a report that did not
/// land, or a host that restarted -- has the same `seq` and is kept once.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Line {
    pub seq: u64,
    pub at: String,
    pub session_id: String,
    pub from: &'static str,
    pub text: String,
    /// On an `instruction` that is a command's text as the session took it:
    /// that command's id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LineReport<'a> {
    pub agent_id: &'a str,
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
    /// `Some(false)`: a message written to its session and not yet taken,
    /// which is reported again once it is settled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taken: Option<bool>,
}

impl CommandReport {
    pub fn new(command_id: String, error: Option<String>) -> Self {
        CommandReport {
            command_id,
            ok: error.is_none(),
            error,
            taken: None,
        }
    }

    pub fn written(command_id: String) -> Self {
        CommandReport {
            taken: Some(false),
            ..CommandReport::new(command_id, None)
        }
    }

    pub fn settled(&self) -> bool {
        self.taken != Some(false)
    }
}

/// The account's usage, as a session's `rate_limit_event` said it at
/// `read_at`. A window the event did not carry is left out, never sent as 0.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub read_at: String,
    #[serde(flatten)]
    pub windows: Windows,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Windows {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<Window>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seven_day: Option<Window>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    pub utilization: f64,
    pub resets_at: String,
}

/// Tokens a session has used, its subagents' included, as Claude Code
/// counts them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tokens {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

impl Tokens {
    pub fn add(self, other: Tokens) -> Tokens {
        Tokens {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cache_creation_input_tokens: self
                .cache_creation_input_tokens
                .saturating_add(other.cache_creation_input_tokens),
            cache_read_input_tokens: self
                .cache_read_input_tokens
                .saturating_add(other.cache_read_input_tokens),
        }
    }
}

/// A session's running total, not what it used since the last report: the
/// service keeps the largest of each count it is told, so a total told again
/// -- after a reconnect, or by a host that restarted and read it again -- is
/// kept once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenReport {
    pub agent_id: String,
    pub session_id: String,
    #[serde(flatten)]
    pub tokens: Tokens,
}

/// The most session totals the service takes in one report.
pub const MOST_TOKENS: usize = 256;

/// The lines read for each agent, by agent id.
pub type Lines = [(String, Vec<Line>)];

pub fn line_reports(lines: &Lines) -> Vec<LineReport<'_>> {
    lines
        .iter()
        .flat_map(|(agent_id, lines)| lines.iter().map(move |line| LineReport { agent_id, line }))
        .collect()
}

/// What the socket carries up: a report, or a ping that keeps API Gateway
/// from closing a socket it thinks idle.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Up<'a> {
    #[serde(rename_all = "camelCase")]
    Report {
        agents: &'a [AgentReport],
        lines: Vec<LineReport<'a>>,
        command_results: &'a [CommandReport],
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<&'a Usage>,
        #[serde(skip_serializing_if = "<[TokenReport]>::is_empty")]
        tokens: &'a [TokenReport],
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
            "id":"c1","kind":"create","agentId":"w1","name":"ana","later":"ignored"
        }))
        .unwrap();
        assert_eq!(create.name.as_deref(), Some("ana"));

        let start: Command = serde_json::from_value(json!({
            "id":"c2","kind":"start","agentId":"w1","sessionId":"s1","resume":true,"text":"go"
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
        assert!(start.context.is_none() && start.env.is_none());

        let given: Command = serde_json::from_value(json!({
            "id":"c3","kind":"start","agentId":"w1","sessionId":"s1","resume":false,
            "context":{"key":"global","version":4,"files":[{"path":"skills/a/SKILL.md","content":"a","executable":false}]},
            "env":{"DS_URL":"https://ds"}
        }))
        .unwrap();
        assert_eq!(given.context.unwrap().version, 4);
        assert_eq!(given.env.unwrap()["DS_URL"], "https://ds");
    }

    #[test]
    fn a_socket_message_is_a_command_or_passed_over() {
        let down: Down = serde_json::from_str(
            r#"{"type":"command","command":{"id":"c1","kind":"stop","agentId":"w1"}}"#,
        )
        .unwrap();
        let Down::Command { command } = down else {
            panic!("{down:?}")
        };
        assert_eq!((command.id.as_str(), command.kind.as_str()), ("c1", "stop"));
        for other in [r#"{"type":"pong"}"#, r#"{"type":"agent","agent":{}}"#] {
            assert!(matches!(
                serde_json::from_str::<Down>(other).unwrap(),
                Down::Other
            ));
        }
        assert!(serde_json::from_str::<Down>("not json").is_err());
    }

    #[test]
    fn a_report_is_agents_lines_results_and_usage_under_its_type() {
        let agents = [
            AgentReport {
                tasks: 2,
                told: Some(false),
                ..AgentReport::new("w1".into(), State::Working, Some("s1".into()))
            },
            AgentReport::new("w2".into(), State::Idle, None),
            AgentReport {
                told: Some(true),
                ..AgentReport::new("w3".into(), State::Waiting, Some("s3".into()))
            },
        ];
        let line = |seq, from, text: &str, command_id: Option<&str>| Line {
            seq,
            at: "2026-10-28T00:00:00.000Z".into(),
            session_id: "s1".into(),
            from,
            text: text.into(),
            command_id: command_id.map(String::from),
        };
        let lines = [(
            "w1".to_string(),
            vec![
                line(4, "instruction", "go", Some("c2")),
                line(5, "agent", "hi", None),
            ],
        )];
        let results = [
            CommandReport::new("c1".into(), Some("no".into())),
            CommandReport::written("c2".into()),
            CommandReport::new("c3".into(), None),
        ];
        let up = Up::Report {
            agents: &agents,
            lines: line_reports(&lines),
            command_results: &results,
            usage: None,
            tokens: &[],
        };
        assert_eq!(
            serde_json::to_value(&up).unwrap(),
            json!({
                "type":"report",
                "agents":[
                    {"id":"w1","state":"working","sessionId":"s1","tasks":2,"told":false},
                    {"id":"w2","state":"idle","sessionId":null,"tasks":0},
                    {"id":"w3","state":"waiting","sessionId":"s3","tasks":0,"told":true}
                ],
                "lines":[
                    {"agentId":"w1","seq":4,"at":"2026-10-28T00:00:00.000Z","sessionId":"s1","from":"instruction","text":"go","commandId":"c2"},
                    {"agentId":"w1","seq":5,"at":"2026-10-28T00:00:00.000Z","sessionId":"s1","from":"agent","text":"hi"}
                ],
                "commandResults":[
                    {"commandId":"c1","ok":false,"error":"no"},
                    {"commandId":"c2","ok":true,"taken":false},
                    {"commandId":"c3","ok":true}
                ]
            })
        );
        let usage = Usage {
            read_at: "2026-11-06T08:28:17.268Z".into(),
            windows: Windows {
                five_hour: Some(Window {
                    utilization: 0.48,
                    resets_at: "2026-11-06T10:00:00.000Z".into(),
                }),
                seven_day: None,
            },
        };
        let up = Up::Report {
            agents: &[],
            lines: vec![],
            command_results: &[],
            usage: Some(&usage),
            tokens: &[],
        };
        assert_eq!(
            serde_json::to_value(&up).unwrap()["usage"],
            json!({"readAt":"2026-11-06T08:28:17.268Z","fiveHour":{"utilization":0.48,"resetsAt":"2026-11-06T10:00:00.000Z"}}),
            "a window not read is left out"
        );
        assert!(serde_json::to_value(&up).unwrap().get("tokens").is_none());
        let tokens = [TokenReport {
            agent_id: "w1".into(),
            session_id: "s1".into(),
            tokens: Tokens {
                input_tokens: 80,
                output_tokens: 19491,
                cache_creation_input_tokens: 71906,
                cache_read_input_tokens: 1976024,
            },
        }];
        let up = Up::Report {
            agents: &[],
            lines: vec![],
            command_results: &[],
            usage: None,
            tokens: &tokens,
        };
        assert_eq!(
            serde_json::to_value(&up).unwrap()["tokens"],
            json!([{"agentId":"w1","sessionId":"s1","inputTokens":80,"outputTokens":19491,
                "cacheCreationInputTokens":71906,"cacheReadInputTokens":1976024}])
        );
        assert_eq!(
            serde_json::to_value(Up::Ping).unwrap(),
            json!({"type":"ping"})
        );
    }
}
