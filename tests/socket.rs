//! The built binary, running on, takes a command pushed on its socket at once
//! and reports on the socket as soon as what it did shows: well inside the 30
//! seconds between heartbeats. The service is a stand-in: an HTTP heartbeat
//! that names a socket, and the socket.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use std::{fs, thread};
use tungstenite::handshake::server::{Request, Response};
use tungstenite::Message;

/// Answers its opening message, and ends its turn.
const CLAUDE: &str = r#"#!/bin/sh
dir="$CLAUDE_CONFIG_DIR/projects/-fake"; mkdir -p "$dir"; t="$dir/$3.jsonl"
printf '%s\n' "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"$4\"}}" >> "$t"
printf '%s\n' '{"type":"assistant","message":{"id":"m0","content":[{"type":"text","text":"ready"}],"stop_reason":"end_turn"}}' >> "$t"
sleep 60
"#;

/// Answers every heartbeat with the socket's url and nothing else to do.
fn heartbeats(listener: TcpListener, socket: String) {
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut reader = BufReader::new(stream.unwrap());
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
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let reply =
                json!({"contract":1,"heartbeatInterval":30,"socketUrl":socket,"commands":[]})
                    .to_string();
            let _ = write!(
                reader.get_mut(),
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
}

struct Machine {
    dir: PathBuf,
    child: Child,
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = Command::new("tmux")
            .env_remove("TMUX")
            .env("TMUX_TMPDIR", self.dir.join("tmux"))
            .args(["-L", "worker-host", "kill-server"])
            .output();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

type Socket = tungstenite::WebSocket<TcpStream>;

/// Reads reports until one satisfies `done`, failing after `within`.
fn report_until(socket: &mut Socket, within: Duration, done: impl Fn(&Value) -> bool) -> Value {
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

fn result(report: &Value, command: &str) -> Option<Value> {
    report["commandResults"]
        .as_array()?
        .iter()
        .find(|r| r["commandId"] == command)
        .cloned()
}

fn push(socket: &mut Socket, command: Value) {
    socket
        .send(Message::text(
            json!({"type":"command","command":command}).to_string(),
        ))
        .unwrap();
}

#[test]
// tungstenite's callback answers a refusal with a whole HTTP response.
#[allow(clippy::result_large_err)]
fn a_command_pushed_on_the_socket_is_taken_and_reported_at_once() {
    // Short, since a unix socket's path is limited to about a hundred bytes.
    let dir = std::env::temp_dir().join(format!("wh-{}-sock", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    for sub in ["tmux", "bin", "home"] {
        fs::create_dir_all(dir.join(sub)).unwrap();
    }
    fs::write(dir.join("bin/claude"), CLAUDE).unwrap();
    fs::set_permissions(dir.join("bin/claude"), fs::Permissions::from_mode(0o755)).unwrap();

    let sockets = TcpListener::bind("127.0.0.1:0").unwrap();
    let socket_url = format!("ws://{}/beta", sockets.local_addr().unwrap());
    let http = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", http.local_addr().unwrap());
    heartbeats(http, socket_url);

    let path = format!(
        "{}:{}",
        dir.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let child = Command::new(env!("CARGO_BIN_EXE_worker-host"))
        .env_clear()
        .env("PATH", path)
        .env("HOME", dir.join("home"))
        .env("XDG_CONFIG_HOME", dir.join("home/.config"))
        .env("TMUX_TMPDIR", dir.join("tmux"))
        .env("WORKER_HOST_SERVICE_URL", &url)
        .env("WORKER_HOST_TOKEN", "the-hosts-own")
        .env("WORKER_HOST_ID", "studio")
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _machine = Machine { dir, child };

    let (stream, _) = sockets.accept().unwrap();
    let (mut path, mut authorization) = (String::new(), String::new());
    let mut socket = tungstenite::accept_hdr(stream, |req: &Request, res: Response| {
        path = req.uri().to_string();
        authorization = req.headers()["authorization"].to_str().unwrap().into();
        Ok(res)
    })
    .unwrap();
    assert_eq!(path, "/beta?hostId=studio");
    assert_eq!(authorization, "Bearer the-hosts-own");
    let opening = report_until(&mut socket, Duration::from_secs(5), |_| true);
    assert_eq!(opening["workers"], json!([]), "a whole report on opening");

    let quick = Duration::from_secs(10);
    push(
        &mut socket,
        json!({"id":"c1","kind":"create","workerId":"w1","name":"ana","role":"x","repositories":[],"credential":"wkr_w1_x"}),
    );
    let created = report_until(&mut socket, quick, |r| result(r, "c1").is_some());
    assert_eq!(result(&created, "c1").unwrap()["ok"], true, "{created}");
    assert_eq!(
        created["workers"],
        json!([{"id":"w1","state":"idle","sessionId":null}])
    );

    push(
        &mut socket,
        json!({"id":"c2","kind":"start","workerId":"w1","sessionId":"5a1e2b3c-4d5e-4f60-8a7b-8c9d0e1f2a3b","resume":false,"text":"hello"}),
    );
    // Its result, its turn ending and what it said may come in one report
    // or in several.
    let (mut ok, mut waiting, mut lines) = (None, false, vec![]);
    let deadline = Instant::now() + quick;
    while ok.is_none() || !waiting || lines.len() < 2 {
        assert!(Instant::now() < deadline, "{ok:?} {waiting} {lines:?}");
        let report = report_until(&mut socket, quick, |_| true);
        ok = ok.or(result(&report, "c2").map(|r| r["ok"].clone()));
        waiting |= report["workers"][0]["state"] == "waiting";
        lines.extend(report["lines"].as_array().cloned().unwrap_or_default());
    }
    assert_eq!(ok, Some(json!(true)));
    assert_eq!(
        lines,
        [
            json!({"workerId":"w1","seq":0,"at":lines[0]["at"],"sessionId":"5a1e2b3c-4d5e-4f60-8a7b-8c9d0e1f2a3b","from":"instruction","text":"hello"}),
            json!({"workerId":"w1","seq":1,"at":lines[1]["at"],"sessionId":"5a1e2b3c-4d5e-4f60-8a7b-8c9d0e1f2a3b","from":"agent","text":"ready"}),
        ]
    );

    push(
        &mut socket,
        json!({"id":"c3","kind":"stop","workerId":"w1"}),
    );
    let stopped = report_until(&mut socket, quick, |r| result(r, "c3").is_some());
    assert_eq!(result(&stopped, "c3").unwrap()["ok"], true, "{stopped}");
    assert_eq!(
        stopped["workers"],
        json!([{"id":"w1","state":"idle","sessionId":null}])
    );

    // Pushed again, as the service would until a report of it lands, it is
    // answered and not done again.
    push(
        &mut socket,
        json!({"id":"c2","kind":"start","workerId":"w1","sessionId":"5a1e2b3c-4d5e-4f60-8a7b-8c9d0e1f2a3b","resume":false,"text":"hello"}),
    );
    let again = report_until(&mut socket, quick, |r| result(r, "c2").is_some());
    assert_eq!(again["workers"][0]["state"], "idle", "{again}");
}
