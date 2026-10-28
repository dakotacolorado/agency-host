//! A worker's credential reaches its session's environment and nowhere else a
//! person or another program could read it: not the host's log, not the
//! transcript it sends up, not a process's arguments, not the prompt. The host
//! here is the built binary, sending heartbeats to a stand-in service, on a
//! tmux server under a TMUX_TMPDIR of the test's own.

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::{fs, thread};

const WORKER: &str = "0b5e1d2c-3f4a-4b6c-8d7e-9f0a1b2c3d4e";
const SESSION: &str = "5a1e2b3c-4d5e-4f60-8a7b-8c9d0e1f2a3b";
const FIRST: &str =
    "wkr_0b5e1d2c-3f4a-4b6c-8d7e-9f0a1b2c3d4e_Zm9yLXRoZS10ZXN0LW9ubHktbm90LWEtcmVhbC1vbmUx";

/// Records what it was started with, then says its token and puts it in a
/// command, as a worker that printed it would.
const CLAUDE: &str = r#"#!/bin/sh
dir="$CLAUDE_CONFIG_DIR/projects/-fake"; mkdir -p "$dir"; t="$dir/$3.jsonl"
printf '%s\n%s\n%s\n' "$WORKER_SERVICE_URL" "$WORKER_ID" "$WORKER_TOKEN" > "$dir/$3.env"
printf '%s\n' "{\"type\":\"assistant\",\"message\":{\"id\":\"m0\",\"content\":[{\"type\":\"text\",\"text\":\"mine is $WORKER_TOKEN\"},{\"type\":\"tool_use\",\"name\":\"Bash\",\"input\":{\"command\":\"curl -H 'Authorization: Bearer $WORKER_TOKEN' $WORKER_SERVICE_URL/fleet/hosts\"}}],\"stop_reason\":\"end_turn\"}}" >> "$t"
sleep 60
"#;

fn create() -> String {
    format!(
        r#"{{"id":"c1","kind":"create","workerId":"{WORKER}","name":"ana","role":"frontend","repositories":[],"credential":"{FIRST}"}}"#
    )
}

fn start() -> String {
    format!(
        r#"{{"id":"c2","kind":"start","workerId":"{WORKER}","sessionId":"{SESSION}","resume":false,"text":"wait"}}"#
    )
}

/// Answers each heartbeat in turn: the create, the same create again with the
/// start, then nothing. Hands back every request, head and body.
fn serve(listener: TcpListener) -> Arc<Mutex<Vec<String>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let kept = seen.clone();
    thread::spawn(move || {
        for (n, stream) in listener.incoming().enumerate() {
            let commands = match n {
                0 => create(),
                1 => format!("{},{}", create(), start()),
                _ => String::new(),
            };
            let request = answer(
                stream.unwrap(),
                &format!(r#"{{"contract":1,"commands":[{commands}],"heartbeatInterval":1}}"#),
            );
            kept.lock().unwrap().push(request);
        }
    });
    seen
}

fn answer(stream: TcpStream, reply: &str) -> String {
    let mut reader = BufReader::new(stream);
    let (mut request, mut length) = (String::new(), 0);
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = v.trim().parse().unwrap();
        }
        request.push_str(&line);
        if line == "\r\n" {
            break;
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    request.push_str(&String::from_utf8(body).unwrap());
    write!(
        reader.get_mut(),
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
        reply.len()
    )
    .unwrap();
    request
}

struct Machine(PathBuf);

impl Machine {
    fn tmux(&self) -> Command {
        let mut command = Command::new("tmux");
        command
            .env_remove("TMUX")
            .env("TMUX_TMPDIR", self.0.join("tmux"))
            .args(["-L", "worker-host"]);
        command
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = self.tmux().arg("kill-server").output();
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_worker_has_its_credential_in_its_environment_and_nowhere_else() {
    // Short, since a unix socket's path is limited to about a hundred bytes.
    let machine = Machine(std::env::temp_dir().join(format!("ah-{}-cred", std::process::id())));
    let dir = &machine.0;
    let _ = fs::remove_dir_all(dir);
    let (bin, home) = (dir.join("bin"), dir.join("home"));
    for d in [&bin, &home, &dir.join("tmux")] {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(bin.join("claude"), CLAUDE).unwrap();
    fs::set_permissions(bin.join("claude"), fs::Permissions::from_mode(0o755)).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = serve(listener);
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(env!("CARGO_BIN_EXE_worker-host"))
        .env_clear()
        .env("PATH", path)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("TMUX_TMPDIR", dir.join("tmux"))
        .env("WORKER_HOST_SERVICE_URL", &url)
        .env("WORKER_HOST_TOKEN", "the-hosts-own")
        .env("WORKER_HOST_ID", "test")
        .env("WORKER_HOST_CAPACITY", "1")
        .args(["--heartbeats", "4"])
        .output()
        .unwrap();
    let log =
        String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{log}");

    let worker = home.join("workers/ana");
    let state = worker.join("home/.claude");
    let env = state.join(format!("projects/-fake/{SESSION}.env"));
    assert_eq!(
        fs::read_to_string(&env).unwrap(),
        format!("{url}\n{WORKER}\n{FIRST}\n"),
        "the session has the service, its id, and the credential it was sent"
    );

    // The arguments of every process on the machine, while the worker runs.
    let ps = Command::new("ps")
        .args(["-axww", "-o", "args="])
        .output()
        .unwrap();
    let ps = String::from_utf8_lossy(&ps.stdout);
    assert!(ps.contains(SESSION), "the worker is running: {ps}");
    let environment = machine
        .tmux()
        .args(["show-environment", "-t", "worker-ana"])
        .output()
        .unwrap();
    let environment = String::from_utf8_lossy(&environment.stdout);
    assert!(
        environment.contains(&format!("WORKER_ID={WORKER}")),
        "{environment}"
    );

    let kept = worker.join("credential");
    assert_eq!(fs::read_to_string(&kept).unwrap(), FIRST);
    assert_eq!(
        fs::metadata(&kept).unwrap().permissions().mode() & 0o777,
        0o600,
        "readable by this user alone"
    );
    let prompt = fs::read_to_string(worker.join("prompt.md")).unwrap()
        + &fs::read_to_string(state.join("CLAUDE.md")).unwrap();

    let requests = requests.lock().unwrap();
    assert!(requests
        .iter()
        .all(|r| r.contains("fleet-contract: 1\r\n") && r.contains("Bearer the-hosts-own")));
    let heartbeats: Vec<Value> = requests
        .iter()
        .filter_map(|r| r.split_once("\r\n\r\n"))
        .map(|(_, body)| serde_json::from_str::<Value>(body).unwrap())
        .collect();
    let results: Vec<&Value> = heartbeats
        .iter()
        .flat_map(|h| h["commandResults"].as_array().unwrap())
        .collect();
    assert!(
        results
            .iter()
            .any(|r| r["commandId"] == "c1" && r["ok"] == true)
            && results
                .iter()
                .any(|r| r["commandId"] == "c2" && r["ok"] == true),
        "{results:?}"
    );
    assert_eq!(
        heartbeats.last().unwrap()["workers"],
        serde_json::json!([{"id": WORKER, "state": "waiting", "sessionId": SESSION}])
    );
    let transcript: Vec<String> = heartbeats
        .iter()
        .flat_map(|h| h["lines"].as_array().cloned().unwrap_or_default())
        .inspect(|line| assert_eq!(line["workerId"], WORKER))
        .map(|line| line["text"].as_str().unwrap().to_string())
        .collect();
    assert!(
        transcript.contains(&"mine is [credential]".to_string()),
        "{transcript:?}"
    );
    assert!(
        transcript.iter().any(|t| t.contains("Bearer [credential]")),
        "{transcript:?}"
    );

    for (place, text) in [
        ("the log", log.as_str()),
        ("any process's arguments", &ps),
        ("the tmux server's environment", &environment),
        ("the prompt", &prompt),
        ("the heartbeats", &requests.join("\n")),
    ] {
        assert!(
            !text.contains(FIRST),
            "the credential is in {place}: {text}"
        );
    }
}
