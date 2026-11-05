//! A stand-in worker-service for the built binary: an HTTP heartbeat that
//! names a socket, the socket, and a host run against them from a config
//! file of the test's own.

// Each test file uses some of these.
#![allow(dead_code)]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use std::{fs, thread};
use tungstenite::handshake::server::{Request, Response};
use tungstenite::Message;

pub type Socket = tungstenite::WebSocket<TcpStream>;

/// The service's two ends: `url` for the config, `sockets` to accept on.
pub struct Service {
    pub url: String,
    pub sockets: TcpListener,
}

impl Service {
    /// Answers every heartbeat with the socket's url and nothing else.
    pub fn start() -> Service {
        Service::serving(None)
    }

    /// As `start`, and answers every GET with `context`, as a start's
    /// `contextUrl` is answered, or 404 without one.
    pub fn serving(context: Option<Value>) -> Service {
        let sockets = TcpListener::bind("127.0.0.1:0").unwrap();
        let socket_url = format!("ws://{}/beta", sockets.local_addr().unwrap());
        let http = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", http.local_addr().unwrap());
        thread::spawn(move || {
            for stream in http.incoming() {
                let mut reader = BufReader::new(stream.unwrap());
                let mut length = 0;
                let mut get = false;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    get |= line.starts_with("GET ");
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let (status, reply) = match (get, &context) {
                    (false, _) => (
                        "200 OK",
                        json!({"heartbeatInterval":30,"socketUrl":socket_url}),
                    ),
                    (true, Some(context)) => ("200 OK", context.clone()),
                    (true, None) => ("404 Not Found", json!({"error":"no such session"})),
                };
                let reply = reply.to_string();
                let _ = write!(
                    reader.get_mut(),
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        Service { url, sockets }
    }

    /// The host's next socket, and the path and authorization it opened with.
    // tungstenite's callback answers a refusal with a whole HTTP response.
    #[allow(clippy::result_large_err)]
    pub fn accept(&self) -> (Socket, String, String) {
        let (stream, _) = self.sockets.accept().unwrap();
        let (mut path, mut authorization) = (String::new(), String::new());
        let socket = tungstenite::accept_hdr(stream, |req: &Request, res: Response| {
            path = req.uri().to_string();
            authorization = req.headers()["authorization"].to_str().unwrap().into();
            Ok(res)
        })
        .unwrap();
        (socket, path, authorization)
    }
}

/// A stand-in for Claude Code, headless over stream-json as the host runs it:
/// it echoes each message back as taken, answers the first `ready` and every
/// one after `heard`, and ends each turn.
pub const CLAUDE: &str = r#"#!/bin/sh
[ "$1" = -p ] || exit 64
reply=ready
while IFS= read -r line; do
  case "$line" in '{"message":{"content":'*) ;; *) continue ;; esac
  printf '%s
' "$line" | sed 's/^{/{"isReplay":true,/'
  printf '%s
' "{\"type\":\"assistant\",\"message\":{\"id\":\"m\",\"content\":[{\"type\":\"text\",\"text\":\"$reply\"}]},\"parent_tool_use_id\":null}"
  printf '%s
' '{"type":"result","subtype":"success","is_error":false}'
  reply=heard
done
"#;

/// Puts `CLAUDE` in `dir/bin`, where `host` finds it.
pub fn install_claude(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(dir.join("bin")).unwrap();
    fs::write(dir.join("bin/claude"), CLAUDE).unwrap();
    fs::set_permissions(dir.join("bin/claude"), fs::Permissions::from_mode(0o755)).unwrap();
}

/// Starts the built binary with `config` as its only settings, under `dir`'s
/// own HOME, with `dir/bin` first on its PATH.
pub fn host(dir: &Path, config: &str) -> Child {
    fs::write(dir.join("config.toml"), config).unwrap();
    let path = format!(
        "{}:{}",
        dir.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new(env!("CARGO_BIN_EXE_worker-host"))
        .env_clear()
        .env("PATH", path)
        .env("HOME", dir.join("home"))
        .arg("--config")
        .arg(dir.join("config.toml"))
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// Reads reports until one satisfies `done`, failing after `within`.
pub fn report_until(socket: &mut Socket, within: Duration, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + within;
    socket
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut seen = vec![];
    loop {
        assert!(Instant::now() < deadline, "no such report in {seen:#?}");
        match socket.read() {
            Ok(Message::Text(text)) => {
                let report: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(report["type"], "report", "{report}");
                if done(&report) {
                    return report;
                }
                seen.push(report);
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => panic!("{e}; read {seen:#?}"),
        }
    }
}

pub fn push(socket: &mut Socket, command: Value) {
    socket
        .send(Message::text(
            json!({"type":"command","command":command}).to_string(),
        ))
        .unwrap();
}

pub fn result(report: &Value, command: &str) -> Option<Value> {
    report["commandResults"]
        .as_array()?
        .iter()
        .find(|r| r["commandId"] == command)
        .cloned()
}
