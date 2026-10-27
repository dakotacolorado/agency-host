use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

use crate::agent::{Agents, Batch};
use crate::config::Config;

/// The agent-service host contract this beat body matches: `reports` arrived
/// in 2 (see agent-service `contracts/`). Raise it only with the body.
pub const CONTRACT: u32 = 2;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Beat<'a> {
    pub host_version: &'a str,
    pub host_commit: &'a str,
    pub agents: Vec<Value>,
    pub reports: &'a [Report],
    /// What agents have done since the last beat that landed, so a transcript
    /// can be read while the agent is still working.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub transcript: Vec<&'a Batch>,
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
    /// `message`: which agent, and what to type to it. Who sent it is the
    /// service's record, not the agent's: a person, the agent above, and an
    /// answer to a question are all carried the same way.
    pub agent_id: Option<String>,
    pub text: Option<String>,
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
    /// The service understood the beat and refused this host: it is too old,
    /// or not allowed. Retrying soon will not change the answer.
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
            Err(ureq::Error::Status(code @ (401 | 403 | 410 | 426), response)) => {
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

/// Placing and stopping are each their own story. Until a kind is handled, the
/// host says so rather than dropping it, so the sender learns the command went
/// nowhere. `ping` does nothing and succeeds, so a whole cycle can be proved
/// before any real kind exists.
pub fn take(command: &Command, agents: &Agents) -> Report {
    let error = match command.kind.as_str() {
        "ping" => None,
        "message" => match (&command.agent_id, &command.text) {
            (Some(agent), Some(text)) => agents.say(agent, text).err(),
            _ => Some("a message needs an agentId and text".into()),
        },
        kind => Some(format!("this host does not handle {kind:?} commands")),
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
            agents: vec![],
            reports: &reports,
            transcript: vec![],
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
            serde_json::json!({"hostVersion":"9.9.9","hostCommit":"0123abc","agents":[],"reports":[{"commandId":"c0","ok":true}]})
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
            agents: vec![],
            reports: &[],
            transcript: vec![],
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
            agents: vec![],
            reports: &[],
            transcript: vec![],
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
                agents: vec![],
                reports: &[],
                transcript: vec![],
            }),
            Err(Failure::Transient(_))
        ));
    }

    /// No tmux server answers on this socket, so the host has no agents.
    fn no_agents() -> Agents {
        Agents::new(
            crate::tmux::Tmux::new(format!("agent-host-test-{}-none", std::process::id())),
            crate::claude::Claude::new("claude", None, "/nonexistent".into()),
        )
    }

    #[test]
    fn a_beat_carries_what_agents_did() {
        let (url, server) = serve_once("200 OK", "{}");
        let batch = Batch {
            agent_id: "a1".into(),
            from: 0,
            to: 120,
            entries: vec![
                crate::agent::Entry::Said { text: "hi".into() },
                crate::agent::Entry::Waiting,
            ],
        };
        assert!(client(url)
            .beat(&Beat {
                host_version: "9.9.9",
                host_commit: "0123abc",
                agents: vec![],
                reports: &[],
                transcript: vec![&batch],
            })
            .is_ok());
        let (_, body) = server.join().unwrap();
        let sent: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            sent["transcript"],
            serde_json::json!([{"agentId":"a1","from":0,"to":120,"entries":[
                {"kind":"said","text":"hi"},{"kind":"waiting"}
            ]}])
        );
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
