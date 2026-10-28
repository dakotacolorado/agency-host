use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::config::Config;
use crate::machine::Sample;
use crate::protocol::{Command, CommandReport, LineReport, WorkerReport};

/// The worker-service host contract this heartbeat body matches. Raise it
/// only with the body, the reply, a command, a report or the socket's
/// messages. Sent as the `fleet-contract` header, as hosts of agent-service
/// did, so the service can refuse a host it cannot speak to with 426.
pub const CONTRACT: u32 = 1;

/// Every field but the reports may be left out, and one left out keeps the
/// value the service last had: a host that could not read something says
/// nothing about it rather than sending a zero or an empty list.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Heartbeat<'a> {
    pub version: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u32>,
    /// Samples taken since the last heartbeat that landed.
    pub samples: &'a [Sample],
    /// Every worker here that is not retired.
    pub workers: &'a [WorkerReport],
    /// What workers have done since the last heartbeat that landed, so a
    /// transcript can be read while the worker is still working.
    pub lines: Vec<LineReport<'a>>,
    pub command_results: &'a [CommandReport],
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reply {
    #[serde(default)]
    pub commands: Vec<Command>,
    pub heartbeat_interval: Option<f64>,
    /// Where to open the socket the service pushes commands on.
    pub socket_url: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{line_reports, Line, State};
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
            tmux_socket: "worker-host-test".into(),
            workers_dir: "/nonexistent".into(),
        })
    }

    #[test]
    fn a_heartbeat_carries_reports_up_and_brings_commands_and_the_socket_down() {
        let (url, server) = serve_once(
            "200 OK",
            r#"{"contract":1,"heartbeatInterval":30,"socketUrl":"wss://socket.example/beta",
                "commands":[{"id":"c1","kind":"stop","workerId":"w1"}]}"#,
        );
        let results = [CommandReport::new("c0".into(), None)];
        let workers = [WorkerReport {
            id: "w1".into(),
            state: State::Waiting,
            session_id: Some("s1".into()),
        }];
        let lines = [(
            "w1".to_string(),
            vec![Line {
                seq: 0,
                at: "2026-10-28T12:00:00.000Z".into(),
                session_id: "s1".into(),
                from: "agent",
                text: "hi".into(),
            }],
        )];
        let Ok(reply) = client(url).heartbeat(&Heartbeat {
            version: "9.9.9",
            workers: &workers,
            lines: line_reports(&lines),
            command_results: &results,
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
                "version":"9.9.9",
                "samples":[],
                "workers":[{"id":"w1","state":"waiting","sessionId":"s1"}],
                "lines":[{"workerId":"w1","seq":0,"at":"2026-10-28T12:00:00.000Z","sessionId":"s1","from":"agent","text":"hi"}],
                "commandResults":[{"commandId":"c0","ok":true}]
            })
        );
        assert_eq!(reply.heartbeat_interval, Some(30.0));
        assert_eq!(
            reply.socket_url.as_deref(),
            Some("wss://socket.example/beta")
        );
        assert_eq!(reply.commands[0].id, "c1");
        assert_eq!(reply.commands[0].worker_id.as_deref(), Some("w1"));
    }

    #[test]
    fn an_unsupported_contract_is_refused_with_the_minimum() {
        let (url, _) = serve_once(
            "426 Upgrade Required",
            r#"{"error":"This host speaks contract 0, and the oldest this service accepts is 1. Upgrade worker-host.","minimum":1,"current":1}"#,
        );
        match client(url).heartbeat(&Heartbeat {
            version: "0.1.0",
            ..Default::default()
        }) {
            Err(Failure::Unsupported { minimum: 1, error }) => assert!(error.contains("Upgrade")),
            _ => panic!("expected an unsupported contract"),
        }
    }

    #[test]
    fn any_other_refusal_keeps_the_reason() {
        let (url, _) = serve_once("403 Forbidden", r#"{"error":"not this host"}"#);
        match client(url).heartbeat(&Heartbeat {
            version: "0.1.0",
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
                version: "0.1.0",
                ..Default::default()
            }),
            Err(Failure::Transient(_))
        ));
    }

    #[test]
    fn a_heartbeat_says_what_the_host_will_run_and_how_loaded_it_is() {
        let (url, server) = serve_once("200 OK", "{}");
        let labels = ["macos".to_string(), "repo:workplace-ui".to_string()];
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
                version: "9.9.9",
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
            serde_json::json!(["macos", "repo:workplace-ui"])
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
}
