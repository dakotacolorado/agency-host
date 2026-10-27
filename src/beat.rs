use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::agent::{Agents, Line, Placement};
use crate::config::Config;
use crate::machine::Sample;

/// The agent-service host contract this beat body matches: `reports` arrived
/// in 2, `labels`, `sessions` and `samples` in 3, and `capacity` in 4 (see
/// agent-service `contracts/`). Raise it only with the body.
pub const CONTRACT: u32 = 4;

/// Every field but the agents and reports may be left out, and one left out
/// keeps the value the service last had: a host that could not read something
/// says nothing about it rather than sending a zero or an empty list.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Beat<'a> {
    pub host_version: &'a str,
    pub host_commit: &'a str,
    pub agents: Vec<Present<'a>>,
    pub reports: &'a [Report],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<&'a [String]>,
    /// The tmux sessions a person has open on the machine. Agents run on the
    /// host's own server and are listed in `agents`, never here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Vec<Session>>,
    /// Samples taken since the last beat that landed.
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    pub samples: &'a [Sample],
}

#[derive(Debug, PartialEq, Serialize)]
pub struct Session {
    pub name: String,
}

/// An agent whose session is running here, by the id the service gave it.
/// Always `working`: whether one that has stopped has finished or is asking is
/// not something its transcript says, so the host does not guess.
#[derive(Debug, Serialize)]
pub struct Present<'a> {
    pub id: String,
    pub state: &'static str,
    /// What it has done since the last beat that landed, so a transcript can
    /// be read while the agent is still working.
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    pub transcript: &'a [Line],
}

impl<'a> Present<'a> {
    pub fn running(id: String, transcript: &'a [Line]) -> Self {
        Present {
            id,
            state: "working",
            transcript,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reply {
    #[serde(default)]
    pub commands: Vec<Command>,
    pub come_back_in: Option<f64>,
}

/// What routes a command, and the fields of each kind handled so far; the
/// rest arrive with the stories that handle them.
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
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub command_id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Unsupported {
    error: String,
    minimum: u32,
}

pub enum Failure {
    /// The service no longer accepts the contract this host speaks.
    Unsupported { minimum: u32, error: String },
    /// The service refused the beat or this host: it is too old, not allowed,
    /// or configured with something the service cannot take, such as a label
    /// it cannot match on. Retrying soon will not change the answer.
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
            url: format!("{}/fleet/hosts/{}/beat", config.service_url, config.host_id),
            authorization: format!("Bearer {}", config.token),
        }
    }

    pub fn beat(&self, beat: &Beat) -> Result<Reply, Failure> {
        let response = self
            .agent
            .post(&self.url)
            .set("Authorization", &self.authorization)
            .set("fleet-contract", &CONTRACT.to_string())
            .send_json(beat);
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
/// succeeds, so a whole cycle can be proved without an agent.
pub fn take(command: &Command, agents: &Agents) -> Report {
    let agent = command.agent_id.as_deref();
    let text = command.text.as_deref();
    let error = match (command.kind.as_str(), agent) {
        ("ping", _) => None,
        ("start", Some(agent)) => agents
            .start(
                agent,
                &Placement {
                    repository: command.repository.as_deref(),
                    branch: command.branch.as_deref(),
                    brief: command.brief.as_deref(),
                },
            )
            .err(),
        ("interrupt", Some(agent)) => agents.interrupt(agent, text).err(),
        ("message", Some(agent)) => match text {
            Some(text) => agents.say(agent, text).err(),
            None => Some("a message needs text".into()),
        },
        (kind @ ("start" | "interrupt" | "message"), None) => {
            Some(format!("{kind:?} needs an agentId"))
        }
        (kind, _) => Some(format!("this host does not handle {kind:?} commands")),
    };
    Report {
        command_id: command.id.clone(),
        ok: error.is_none(),
        error,
    }
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
        })
    }

    #[test]
    fn a_beat_carries_reports_up_and_brings_commands_down() {
        let (url, server) = serve_once(
            "200 OK",
            r#"{"commands":[{"id":"c1","kind":"place","agent":{"id":"a1"}}],"comeBackIn":3}"#,
        );
        let reports = [Report {
            command_id: "c0".into(),
            ok: true,
            error: None,
        }];
        let Ok(reply) = client(url).beat(&Beat {
            host_version: "9.9.9",
            host_commit: "0123abc",
            agents: vec![Present::running("a1".into(), &[])],
            reports: &reports,
            ..Default::default()
        }) else {
            panic!("beat failed")
        };
        let (head, body) = server.join().unwrap();

        assert!(head.starts_with("POST /fleet/hosts/studio/beat "), "{head}");
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
                "hostCommit":"0123abc",
                "agents":[{"id":"a1","state":"working"}],
                "reports":[{"commandId":"c0","ok":true}]
            })
        );
        assert_eq!(reply.come_back_in, Some(3.0));
        assert_eq!(reply.commands[0].id, "c1");
    }

    #[test]
    fn an_unsupported_contract_is_refused_with_the_minimum() {
        let (url, _) = serve_once(
            "426 Upgrade Required",
            r#"{"error":"This host speaks contract 2, and the oldest this service accepts is 3. Upgrade agent-host.","minimum":3,"current":4}"#,
        );
        match client(url).beat(&Beat {
            host_version: "0.1.0",
            host_commit: "dirty",
            ..Default::default()
        }) {
            Err(Failure::Unsupported { minimum: 3, error }) => assert!(error.contains("Upgrade")),
            _ => panic!("expected an unsupported contract"),
        }
    }

    #[test]
    fn any_other_refusal_keeps_the_reason() {
        let (url, _) = serve_once("403 Forbidden", r#"{"error":"not this host"}"#);
        match client(url).beat(&Beat {
            host_version: "0.1.0",
            host_commit: "dirty",
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
            client(url).beat(&Beat {
                host_version: "0.1.0",
                host_commit: "dirty",
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
        )
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
            .beat(&Beat {
                host_version: "9.9.9",
                host_commit: "0123abc",
                agents: vec![
                    Present::running("a1".into(), &lines),
                    Present::running("a2".into(), &[])
                ],
                ..Default::default()
            })
            .is_ok());
        let (_, body) = server.join().unwrap();
        let sent: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            sent["agents"],
            serde_json::json!([
                {"id":"a1","state":"working","transcript":[
                    {"seq":0,"at":"2026-10-27T12:00:00.000Z","from":"agent","text":"hi"}
                ]},
                {"id":"a2","state":"working"}
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
            .beat(&Beat {
                host_version: "9.9.9",
                host_commit: "0123abc",
                capacity: Some(2),
                labels: Some(&labels),
                sessions: Some(vec![Session {
                    name: "dotfiles".into(),
                }]),
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
        assert_eq!(sent["sessions"], serde_json::json!([{"name":"dotfiles"}]));
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
            .beat(&Beat {
                capacity: Some(0),
                labels: Some(&[]),
                sessions: Some(vec![]),
                ..Default::default()
            })
            .is_ok());
        let (_, body) = server.join().unwrap();
        let sent: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(sent["capacity"], 0);
        assert_eq!(sent["labels"], serde_json::json!([]));
        assert_eq!(sent["sessions"], serde_json::json!([]));
    }

    #[test]
    fn a_label_the_service_refuses_is_reported_with_its_reason() {
        let reason = r#"{"error":"labels.0: Invalid string: must match pattern /^[a-z0-9][a-z0-9:._-]{0,63}$/"}"#;
        let (url, _) = serve_once("400 Bad Request", reason);
        let labels = ["GPU".to_string()];
        match client(url).beat(&Beat {
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
        let report = take(&command, &no_agents());
        assert!(!report.ok);
        assert!(report.error.unwrap().contains("no agent a9"));
    }

    #[test]
    fn a_message_without_its_fields_is_refused() {
        let command: Command = serde_json::from_str(r#"{"id":"c1","kind":"message"}"#).unwrap();
        assert!(!take(&command, &no_agents()).ok);
    }

    #[test]
    fn an_unhandled_command_is_reported_not_dropped() {
        let command: Command = serde_json::from_str(r#"{"id":"c1","kind":"stop"}"#).unwrap();
        let report = take(&command, &no_agents());
        assert_eq!(report.command_id, "c1");
        assert!(!report.ok);
        assert_eq!(
            report.error.as_deref(),
            Some(r#"this host does not handle "stop" commands"#)
        );
    }

    #[test]
    fn a_start_that_fails_is_reported_with_its_reason() {
        let command: Command =
            serde_json::from_str(r#"{"id":"c1","kind":"start","agentId":"a1"}"#).unwrap();
        let report = take(&command, &no_agents());
        assert!(!report.ok);
        assert!(
            report.error.unwrap().contains("not installed"),
            "the reason is carried up"
        );
    }

    #[test]
    fn a_command_for_an_agent_names_one() {
        for kind in ["start", "interrupt", "message"] {
            let command: Command =
                serde_json::from_str(&format!(r#"{{"id":"c1","kind":"{kind}","text":"hi"}}"#))
                    .unwrap();
            let report = take(&command, &no_agents());
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
        let report = take(&command, &no_agents());
        assert!(!report.ok);
        assert!(report.error.unwrap().contains("no agent a9"));
    }

    #[test]
    fn a_ping_succeeds() {
        let command: Command = serde_json::from_str(r#"{"id":"c1","kind":"ping"}"#).unwrap();
        assert_eq!(
            take(&command, &no_agents()),
            Report {
                command_id: "c1".into(),
                ok: true,
                error: None
            }
        );
    }
}
