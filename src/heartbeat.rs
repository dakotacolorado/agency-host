use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::agent::{Agents, Credential, Deliveries, Line, Placement};
use crate::config::Config;
use crate::machine::Sample;

/// The agent-service host contract this heartbeat body matches: `reports`
/// arrived in 2, `labels`, `sessions` and `samples` in 3, and `capacity` in 4
/// (see agent-service `contracts/`). Raise it only with the body. At 5 the body
/// is unchanged, and each `start` comes with the agent's credential. At 6 the
/// route is `heartbeat`, `reports` are `commandResults`, `sessions` are gone,
/// the states are `running` and `waiting`, the reply says `heartbeatInterval`,
/// and a `start` names its `workItemId`.
pub const CONTRACT: u32 = 6;

/// Every field but the agents and command results may be left out, and one
/// left out keeps the value the service last had: a host that could not read
/// something says nothing about it rather than sending a zero or an empty list.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Heartbeat<'a> {
    pub host_version: &'a str,
    pub agents: Vec<Present<'a>>,
    pub command_results: &'a [Report],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<&'a [String]>,
    /// Samples taken since the last heartbeat that landed.
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    pub samples: &'a [Sample],
}

/// An agent whose session is running here, by the id the service gave it.
/// `waiting` when its last turn ended and nothing has been said to it since:
/// it is waiting on someone, whether it ended on a question or on its work
/// being done, which its transcript does not tell apart, so the host does not
/// guess which. Otherwise `running`.
#[derive(Debug, Serialize)]
pub struct Present<'a> {
    pub id: String,
    pub state: &'static str,
    /// What it has done since the last heartbeat that landed, so a transcript
    /// can be read while the agent is still working.
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    pub transcript: &'a [Line],
}

impl<'a> Present<'a> {
    pub fn running(id: String, waiting: bool, transcript: &'a [Line]) -> Self {
        Present {
            id,
            state: if waiting { "waiting" } else { "running" },
            transcript,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reply {
    #[serde(default)]
    pub commands: Vec<Command>,
    pub heartbeat_interval: Option<f64>,
}

/// What routes a command, and the fields of each kind the host handles.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Command {
    pub id: String,
    pub kind: String,
    /// Which agent, for every kind but `ping`; for `message` and `interrupt`,
    /// what to type to it. Who sent it is the service's record, not the
    /// agent's: a person, the agent above, and an answer to a question are all
    /// carried the same way.
    pub agent_id: Option<String>,
    pub text: Option<String>,
    /// For `start`: what the work is and where it is done, as placed.
    pub repository: Option<String>,
    pub branch: Option<String>,
    pub brief: Option<String>,
    /// For `start`: the agent's own bearer token for the fleet. The same one
    /// comes on every redelivery, and the first is the one kept.
    pub credential: Option<Credential>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub command_id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Report {
    pub fn new(command_id: String, error: Option<String>) -> Self {
        Report {
            command_id,
            ok: error.is_none(),
            error,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Unsupported {
    error: String,
    minimum: u32,
}

pub enum Failure {
    /// The service no longer accepts the contract this host speaks.
    Unsupported { minimum: u32, error: String },
    /// The service refused the heartbeat or this host: it is too old, not
    /// allowed, or configured with something the service cannot take, such as a
    /// label it cannot match on. Retrying soon will not change the answer.
    Refused(u16, String),
    /// Anything that might be different on the next try: no network, a
    /// suspended laptop waking, a service mid-deploy.
    Transient(String),
}

pub struct Client {
    agent: ureq::Agent,
    url: String,
    authorization: String,
}

impl Client {
    pub fn new(config: &Config) -> Self {
        Client {
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(30))
                .build(),
            url: format!(
                "{}/fleet/hosts/{}/heartbeat",
                config.service_url, config.host_id
            ),
            authorization: format!("Bearer {}", config.token),
        }
    }

    pub fn heartbeat(&self, heartbeat: &Heartbeat) -> Result<Reply, Failure> {
        let response = self
            .agent
            .post(&self.url)
            .set("Authorization", &self.authorization)
            .set("fleet-contract", &CONTRACT.to_string())
            .send_json(heartbeat);
        match response {
            Ok(response) => response
                .into_json()
                .map_err(|e| Failure::Transient(format!("unreadable reply: {e}"))),
            Err(ureq::Error::Status(code @ (400 | 401 | 403 | 410 | 426), response)) => {
                let body = response.into_string().unwrap_or_default();
                Err(match serde_json::from_str::<Unsupported>(&body) {
                    Ok(Unsupported { error, minimum }) if code == 426 => {
                        Failure::Unsupported { minimum, error }
                    }
                    _ => Failure::Refused(code, body),
                })
            }
            Err(ureq::Error::Status(code, response)) => Err(Failure::Transient(format!(
                "{code} {}",
                response.into_string().unwrap_or_default()
            ))),
            Err(e) => Err(Failure::Transient(e.to_string())),
        }
    }
}

/// A kind the host does not handle is reported by name rather than dropped,
/// so the sender learns the command went nowhere. `ping` does nothing and
/// succeeds, so a whole cycle can be proved without an agent. A message is
/// reported once it reaches its agent, by `Deliveries::settle`, so none is
/// reported here while it is on its way.
pub fn take(command: &Command, agents: &Agents, deliveries: &mut Deliveries) -> Option<Report> {
    let agent = command.agent_id.as_deref();
    let text = command.text.as_deref();
    if deliveries.holds(&command.id) {
        return None;
    }
    let error = match (command.kind.as_str(), agent) {
        ("ping", _) => None,
        ("start", Some(agent)) => agents
            .start(
                agent,
                &Placement {
                    repository: command.repository.as_deref(),
                    branch: command.branch.as_deref(),
                    brief: command.brief.as_deref(),
                    credential: command.credential.as_ref(),
                },
            )
            .err(),
        ("interrupt", Some(agent)) => agents.interrupt(agent, text).err(),
        ("stop", Some(agent)) => agents.stop(agent).err(),
        ("message", Some(agent)) => match text {
            Some(text) => deliveries.hold(agents, &command.id, agent, text)?.err(),
            None => Some("a message needs text".into()),
        },
        (kind @ ("start" | "interrupt" | "message" | "stop"), None) => {
            Some(format!("{kind:?} needs an agentId"))
        }
        (kind, _) => Some(format!("this host does not handle {kind:?} commands")),
    };
    Some(Report::new(command.id.clone(), error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::thread;

    /// Answers one request with `status` and `body`, and hands back what it was sent.
    fn serve_once(status: &str, body: &str) -> (String, thread::JoinHandle<(String, String)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut head = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            reader.get_mut().write_all(reply.as_bytes()).unwrap();
            (head, String::from_utf8(body).unwrap())
        });
        (url, handle)
    }

    fn client(url: String) -> Client {
        Client::new(&Config {
            service_url: url,
            token: "secret".into(),
            host_id: "studio".into(),
            capacity: None,
            labels: None,
            repositories: Default::default(),
            tmux_socket: "agent-host".into(),
        })
    }

    #[test]
    fn a_beat_carries_command_results_up_and_brings_commands_down() {
        let (url, server) = serve_once(
            "200 OK",
            r#"{"commands":[{"id":"c1","kind":"place","agent":{"id":"a1"}}],"heartbeatInterval":3}"#,
        );
        let reports = [Report {
            command_id: "c0".into(),
            ok: true,
            error: None,
        }];
        let Ok(reply) = client(url).heartbeat(&Heartbeat {
            host_version: "9.9.9",
            agents: vec![Present::running("a1".into(), false, &[])],
            command_results: &reports,
            ..Default::default()
        }) else {
            panic!("heartbeat failed")
        };
        let (head, body) = server.join().unwrap();

        assert!(
            head.starts_with("POST /fleet/hosts/studio/heartbeat "),
            "{head}"
        );
        assert!(head.contains("Authorization: Bearer secret"), "{head}");
        assert!(
            head.contains(&format!("fleet-contract: {CONTRACT}\r\n")),
            "{head}"
        );
        let sent: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            sent,
            serde_json::json!({
                "hostVersion":"9.9.9",
                "agents":[{"id":"a1","state":"running"}],
                "commandResults":[{"commandId":"c0","ok":true}]
            })
        );
        assert_eq!(reply.heartbeat_interval, Some(3.0));
        assert_eq!(reply.commands[0].id, "c1");
    }

    #[test]
    fn an_unsupported_contract_is_refused_with_the_minimum() {
        let (url, _) = serve_once(
            "426 Upgrade Required",
            r#"{"error":"This host speaks contract 2, and the oldest this service accepts is 3. Upgrade agent-host.","minimum":3,"current":4}"#,
        );
        match client(url).heartbeat(&Heartbeat {
            host_version: "0.1.0",
            ..Default::default()
        }) {
            Err(Failure::Unsupported { minimum: 3, error }) => assert!(error.contains("Upgrade")),
            _ => panic!("expected an unsupported contract"),
        }
    }

    #[test]
    fn any_other_refusal_keeps_the_reason() {
        let (url, _) = serve_once("403 Forbidden", r#"{"error":"not this host"}"#);
        match client(url).heartbeat(&Heartbeat {
            host_version: "0.1.0",
            ..Default::default()
        }) {
            Err(Failure::Refused(403, body)) => assert!(body.contains("not this host")),
            _ => panic!("expected a refusal"),
        }
    }

    #[test]
    fn a_service_error_is_worth_retrying() {
        let (url, _) = serve_once("503 Service Unavailable", "{}");
        assert!(matches!(
            client(url).heartbeat(&Heartbeat {
                host_version: "0.1.0",
                ..Default::default()
            }),
            Err(Failure::Transient(_))
        ));
    }

    /// No tmux server answers on this socket, so the host has no agents.
    fn no_agents() -> Agents {
        Agents::new(
            crate::tmux::Tmux::new(format!("agent-host-test-{}-none", std::process::id())),
            crate::claude::Claude::new("/nonexistent/claude", None, "/nonexistent".into()),
            "/nonexistent".into(),
            Default::default(),
            "http://fleet.test",
        )
    }

    /// Takes a command that is settled as soon as it is taken.
    fn take_now(command: &Command) -> Report {
        take(command, &no_agents(), &mut Deliveries::new(Duration::ZERO)).unwrap()
    }

    #[test]
    fn a_beat_carries_what_agents_did_in_each_agents_report() {
        let (url, server) = serve_once("200 OK", "{}");
        let lines = [Line {
            seq: 0,
            at: "2026-10-27T12:00:00.000Z".into(),
            from: "agent",
            text: "hi".into(),
        }];
        assert!(client(url)
            .heartbeat(&Heartbeat {
                host_version: "9.9.9",
                agents: vec![
                    Present::running("a1".into(), false, &lines),
                    Present::running("a2".into(), true, &[])
                ],
                ..Default::default()
            })
            .is_ok());
        let (_, body) = server.join().unwrap();
        let sent: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            sent["agents"],
            serde_json::json!([
                {"id":"a1","state":"running","transcript":[
                    {"seq":0,"at":"2026-10-27T12:00:00.000Z","from":"agent","text":"hi"}
                ]},
                {"id":"a2","state":"waiting"}
            ])
        );
    }

    #[test]
    fn a_beat_says_what_the_host_will_run_and_how_loaded_it_is() {
        let (url, server) = serve_once("200 OK", "{}");
        let labels = ["macos".to_string(), "repo:agent-service".to_string()];
        let samples = [Sample {
            at: "2026-10-27T12:00:00.000Z".into(),
            cpu: 0.25,
            memory_used: 3,
            memory_total: 8,
            disk_used: 40,
            disk_total: 100,
        }];
        assert!(client(url)
            .heartbeat(&Heartbeat {
                host_version: "9.9.9",
                capacity: Some(2),
                labels: Some(&labels),
                samples: &samples,
                ..Default::default()
            })
            .is_ok());
        let (_, body) = server.join().unwrap();
        let sent: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(sent["capacity"], 2);
        assert_eq!(
            sent["labels"],
            serde_json::json!(["macos", "repo:agent-service"])
        );
        assert_eq!(
            sent["samples"],
            serde_json::json!([{"at":"2026-10-27T12:00:00.000Z","cpu":0.25,
                "memoryUsed":3,"memoryTotal":8,"diskUsed":40,"diskTotal":100}])
        );
    }

    #[test]
    fn what_was_read_as_none_is_sent_as_none() {
        let (url, server) = serve_once("200 OK", "{}");
        assert!(client(url)
            .heartbeat(&Heartbeat {
                capacity: Some(0),
                labels: Some(&[]),
                ..Default::default()
            })
            .is_ok());
        let (_, body) = server.join().unwrap();
        let sent: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(sent["capacity"], 0);
        assert_eq!(sent["labels"], serde_json::json!([]));
    }

    #[test]
    fn a_label_the_service_refuses_is_reported_with_its_reason() {
        let reason = r#"{"error":"labels.0: Invalid string: must match pattern /^[a-z0-9][a-z0-9:._-]{0,63}$/"}"#;
        let (url, _) = serve_once("400 Bad Request", reason);
        let labels = ["GPU".to_string()];
        match client(url).heartbeat(&Heartbeat {
            labels: Some(&labels),
            ..Default::default()
        }) {
            Err(Failure::Refused(400, body)) => assert_eq!(body, reason),
            _ => panic!("expected a refusal"),
        }
    }

    #[test]
    fn a_message_for_an_agent_not_here_is_reported() {
        let command: Command =
            serde_json::from_str(r#"{"id":"c1","kind":"message","agentId":"a9","text":"hi"}"#)
                .unwrap();
        let report = take_now(&command);
        assert!(!report.ok);
        assert!(report.error.unwrap().contains("no agent a9"));
    }

    #[test]
    fn a_message_without_its_fields_is_refused() {
        let command: Command = serde_json::from_str(r#"{"id":"c1","kind":"message"}"#).unwrap();
        assert!(!take_now(&command).ok);
    }

    #[test]
    fn an_unhandled_command_is_reported_not_dropped() {
        let command: Command = serde_json::from_str(r#"{"id":"c1","kind":"reboot"}"#).unwrap();
        let report = take_now(&command);
        assert_eq!(report.command_id, "c1");
        assert!(!report.ok);
        assert_eq!(
            report.error.as_deref(),
            Some(r#"this host does not handle "reboot" commands"#)
        );
    }

    #[test]
    fn a_stop_for_an_agent_not_here_is_done() {
        let command: Command =
            serde_json::from_str(r#"{"id":"c1","kind":"stop","agentId":"a9"}"#).unwrap();
        assert_eq!(take_now(&command), Report::new("c1".into(), None));
    }

    #[test]
    fn a_start_that_fails_is_reported_with_its_reason() {
        let command: Command =
            serde_json::from_str(r#"{"id":"c1","kind":"start","agentId":"a1"}"#).unwrap();
        let report = take_now(&command);
        assert!(!report.ok);
        assert!(
            report.error.unwrap().contains("not installed"),
            "the reason is carried up"
        );
    }

    #[test]
    fn a_command_for_an_agent_names_one() {
        for kind in ["start", "interrupt", "message", "stop"] {
            let command: Command =
                serde_json::from_str(&format!(r#"{{"id":"c1","kind":"{kind}","text":"hi"}}"#))
                    .unwrap();
            let report = take_now(&command);
            assert_eq!(
                report.error.as_deref(),
                Some(format!("{kind:?} needs an agentId").as_str())
            );
        }
    }

    #[test]
    fn an_interrupt_for_an_agent_not_here_is_reported() {
        let command: Command =
            serde_json::from_str(r#"{"id":"c1","kind":"interrupt","agentId":"a9"}"#).unwrap();
        let report = take_now(&command);
        assert!(!report.ok);
        assert!(report.error.unwrap().contains("no agent a9"));
    }

    #[test]
    fn a_ping_succeeds() {
        let command: Command = serde_json::from_str(r#"{"id":"c1","kind":"ping"}"#).unwrap();
        assert_eq!(
            take_now(&command),
            Report {
                command_id: "c1".into(),
                ok: true,
                error: None
            }
        );
    }
}
