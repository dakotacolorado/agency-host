//! Two hosts on one machine each report only their own workers, which needs
//! each on a tmux server and a workers folder of its own. Every server here is
//! under a TMUX_TMPDIR of the test's, so the default socket's name can be used
//! without reaching a host that is really running on this machine.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{fs, thread};

struct Machine {
    dir: PathBuf,
    sockets: Vec<String>,
}

impl Machine {
    fn new(name: &str) -> Self {
        // Short, since a unix socket's path is limited to about a hundred bytes.
        let dir = std::env::temp_dir().join(format!("wh-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("tmux")).unwrap();
        Machine {
            dir,
            sockets: vec![],
        }
    }

    fn tmux(&self, socket: &str) -> Command {
        let mut command = Command::new("tmux");
        command
            .env_remove("TMUX")
            .env("TMUX_TMPDIR", self.dir.join("tmux"))
            .args(["-L", socket]);
        command
    }

    /// A worker's folder as a host keeps one, with one session begun.
    fn worker(&self, workers: &Path, id: &str, name: &str) {
        let home = workers.join(name);
        fs::create_dir_all(&home).unwrap();
        let record = json!({"id": id, "name": name, "role": "x", "repositories": []});
        fs::write(home.join("worker.json"), record.to_string()).unwrap();
        let session = json!({"sessionId": format!("s-{id}"), "claude": "c", "from": 0});
        fs::write(home.join("sessions.jsonl"), format!("{session}\n")).unwrap();
    }

    /// A worker's session as a host starts one: tagged with the worker's id.
    fn session(&mut self, socket: &str, id: &str) {
        let out = self
            .tmux(socket)
            .args(["new-session", "-d", "-P", "-F", "#{session_id}"])
            .args(["-s", id, "sleep", "60"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let session = String::from_utf8(out.stdout).unwrap();
        let tagged = self
            .tmux(socket)
            .args(["set-option", "-t", session.trim(), "@worker", id])
            .status()
            .unwrap();
        assert!(tagged.success());
        self.sockets.push(socket.to_string());
    }

    /// The workers one host sends a heartbeat with, as (id, state), run with
    /// `env` and nothing from the person's own config or folders.
    fn heartbeat(&self, env: &[(&str, &str)]) -> Vec<(String, String)> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || serve_one_heartbeat(&listener));
        let home = self.dir.join("home");
        let out = host_command(&self.dir, &home)
            .env("WORKER_HOST_SERVICE_URL", &url)
            .envs(env.iter().copied())
            .args(["--heartbeats", "1"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let heartbeat: Value = serde_json::from_str(&server.join().unwrap()).unwrap();
        heartbeat["workers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| {
                (
                    w["id"].as_str().unwrap().to_string(),
                    w["state"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        for socket in &self.sockets {
            let _ = self.tmux(socket).arg("kill-server").output();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn host_command(dir: &Path, home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_worker-host"));
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("TMUX_TMPDIR", dir.join("tmux"))
        .env("WORKER_HOST_TOKEN", "unused")
        .env("WORKER_HOST_ID", "test");
    command
}

/// Answers one heartbeat with no commands, and hands back its body.
fn serve_one_heartbeat(listener: &TcpListener) -> String {
    let (stream, _) = listener.accept().unwrap();
    let mut reader = BufReader::new(stream);
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
    let reply = r#"{"contract":1,"commands":[],"heartbeatInterval":30}"#;
    write!(
        reader.get_mut(),
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
        reply.len()
    )
    .unwrap();
    String::from_utf8(body).unwrap()
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

#[test]
fn two_hosts_on_one_machine_each_report_only_their_own_workers() {
    let mut machine = Machine::new("two");
    let (beta, prod) = (machine.dir.join("beta"), machine.dir.join("prod"));
    machine.worker(&beta, "b1", "bea");
    machine.worker(&prod, "p1", "pat");
    machine.worker(&prod, "p2", "pia");
    machine.session("beta", "b1");
    machine.session("prod", "p1");
    machine.session("beta", "p2");
    let prod_dir = prod.to_string_lossy().into_owned();
    assert_eq!(
        machine.heartbeat(&[
            ("WORKER_HOST_TMUX_SOCKET", "prod"),
            ("WORKER_HOST_WORKERS_DIR", &prod_dir),
        ]),
        pairs(&[("p1", "working"), ("p2", "interrupted")]),
        "p2's session is not on prod's server, and was not stopped"
    );
    let config = machine.dir.join("home/.config/worker-host/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        &config,
        format!(
            "tmux_socket = \"beta\"\nworkers_dir = \"{}\"\n",
            beta.display()
        ),
    )
    .unwrap();
    assert_eq!(machine.heartbeat(&[]), pairs(&[("b1", "working")]));
}

#[test]
fn a_host_with_no_settings_finds_its_workers_in_home_on_its_own_socket() {
    let mut machine = Machine::new("defaults");
    machine.worker(&machine.dir.join("home/workers"), "w1", "ana");
    machine.worker(&machine.dir.join("home/workers"), "w2", "bo");
    machine.session("worker-host", "w1");
    // On another server: none of this host's.
    machine.session("elsewhere", "w2");
    assert_eq!(
        machine.heartbeat(&[]),
        pairs(&[("w1", "working"), ("w2", "interrupted")])
    );
}
