//! Liveness, and what the host offers: the heartbeat alone keeps a host
//! `connected`, and its reply names the socket everything else goes on.

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::config::Config;
use crate::protocol::Whole;

/// A field left out keeps the value the service last had: a host that could
/// not read something says nothing about it rather than sending a zero or an
/// empty list.
#[derive(Debug, Default, Serialize)]
pub struct Heartbeat<'a> {
    pub version: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reply {
    pub heartbeat_interval: Option<f64>,
    /// Where to open the socket commands and reports go on.
    pub socket_url: Option<String>,
}

pub enum Failure {
    /// The service refused the heartbeat or this host: it is not allowed, or
    /// configured with something the service cannot take, such as a
    /// label it cannot match on. Retrying soon will not change the answer.
    Refused(u16, String),
    /// Anything that might be different on the next try: no network, a
    /// suspended laptop waking, a service mid-deploy.
    Transient(String),
}

pub struct Client {
    agent: ureq::Agent,
    base: String,
    url: String,
    authorization: String,
}

impl Client {
    pub fn new(config: &Config) -> Self {
        Client {
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(30))
                .build(),
            base: config.service_url.trim_end_matches('/').to_string(),
            url: format!("{}/hosts/{}/heartbeat", config.service_url, config.host_id),
            authorization: format!("Bearer {}", config.token),
        }
    }

    /// A start's whole context, read at the `contextUrl` it named.
    pub fn context(&self, path: &str) -> Result<Whole, String> {
        let response = self
            .agent
            .get(&format!("{}{path}", self.base))
            .set("Authorization", &self.authorization)
            .call();
        match response {
            Ok(response) => response
                .into_json()
                .map_err(|e| format!("unreadable context: {e}")),
            Err(ureq::Error::Status(code, response)) => Err(format!(
                "{code} {}",
                response.into_string().unwrap_or_default()
            )),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn heartbeat(&self, heartbeat: &Heartbeat) -> Result<Reply, Failure> {
        let response = self
            .agent
            .post(&self.url)
            .set("Authorization", &self.authorization)
            .send_json(heartbeat);
        match response {
            Ok(response) => response
                .into_json()
                .map_err(|e| Failure::Transient(format!("unreadable reply: {e}"))),
            Err(ureq::Error::Status(code @ (400 | 401 | 403 | 410), response)) => Err(
                Failure::Refused(code, response.into_string().unwrap_or_default()),
            ),
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
            agents_dir: "/nonexistent".into(),
            permissions: Default::default(),
        })
    }

    #[test]
    fn a_heartbeat_says_only_what_the_host_is_and_brings_the_socket_down() {
        let (url, server) = serve_once(
            "200 OK",
            r#"{"heartbeatInterval":30,"socketUrl":"wss://socket.example/beta"}"#,
        );
        let Ok(reply) = client(url).heartbeat(&Heartbeat {
            version: "9.9.9",
            ..Default::default()
        }) else {
            panic!("heartbeat failed")
        };
        let (head, body) = server.join().unwrap();

        assert!(head.starts_with("POST /hosts/studio/heartbeat "), "{head}");
        assert!(head.contains("Authorization: Bearer secret"), "{head}");
        let sent: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(sent, serde_json::json!({"version":"9.9.9"}));
        assert_eq!(reply.heartbeat_interval, Some(30.0));
        assert_eq!(
            reply.socket_url.as_deref(),
            Some("wss://socket.example/beta")
        );
    }

    #[test]
    fn a_whole_context_is_read_at_the_path_the_start_named() {
        let (url, server) = serve_once(
            "200 OK",
            r#"{"context":{"key":"global","version":4,"files":[{"path":"CLAUDE.md","content":"x","executable":false}]},"documents":[]}"#,
        );
        let whole = client(format!("{url}/"))
            .context("/agents/w/sessions/s/context")
            .unwrap();
        let (head, _) = server.join().unwrap();
        assert!(
            head.starts_with("GET /agents/w/sessions/s/context "),
            "{head}"
        );
        assert!(head.contains("Authorization: Bearer secret"), "{head}");
        assert_eq!(whole.context.unwrap().version, 4);
        assert!(whole.documents.is_empty());
    }

    #[test]
    fn a_context_the_service_will_not_give_says_why() {
        let (url, _) = serve_once("409 Conflict", r#"{"error":"not live"}"#);
        let err = client(url).context("/x").unwrap_err();
        assert!(err.starts_with("409 ") && err.contains("not live"), "{err}");
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
    fn a_heartbeat_says_what_the_host_will_run() {
        let (url, server) = serve_once("200 OK", "{}");
        let labels = ["macos".to_string(), "linux".to_string()];
        assert!(client(url)
            .heartbeat(&Heartbeat {
                version: "9.9.9",
                capacity: Some(2),
                labels: Some(&labels),
            })
            .is_ok());
        let (_, body) = server.join().unwrap();
        let sent: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(sent["capacity"], 2);
        assert_eq!(sent["labels"], serde_json::json!(["macos", "linux"]));
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
