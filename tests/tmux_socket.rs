//! Two hosts on one machine each beat with only their own agents, which needs
//! each on a tmux server of its own. Every server here is under a TMUX_TMPDIR
//! of the test's, so the default socket's name can be used without reaching a
//! host that is really running on this machine.

use serde_json::Value;
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
        let dir = std::env::temp_dir().join(format!("ah-{}-{name}", std::process::id()));
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

    /// An agent as a host starts one: a session tagged with the agent's id.
    fn agent(&mut self, socket: &str, id: &str) {
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
            .args(["set-option", "-t", session.trim(), "@agent", id])
            .status()
            .unwrap();
        assert!(tagged.success());
        self.sockets.push(socket.to_string());
    }

    /// The agents one host beats with, run with `env` and nothing from the
    /// person's own config, data or Claude Code.
    fn beat(&self, env: &[(&str, &str)]) -> Vec<String> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || serve_one_beat(&listener));
        let home = self.dir.join("home");
        let out = host_command(&self.dir, &home)
            .env("AGENT_HOST_SERVICE_URL", &url)
            .envs(env.iter().copied())
            .args(["--beats", "1"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let beat: Value = serde_json::from_str(&server.join().unwrap()).unwrap();
        beat["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["id"].as_str().unwrap().to_string())
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
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-host"));
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("TMUX_TMPDIR", dir.join("tmux"))
        .env("AGENT_HOST_TOKEN", "unused")
        .env("AGENT_HOST_ID", "test");
    command
}

/// Answers one beat with no commands, and hands back its body.
fn serve_one_beat(listener: &TcpListener) -> String {
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
    let reply = r#"{"commands":[]}"#;
    write!(
        reader.get_mut(),
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
        reply.len()
    )
    .unwrap();
    String::from_utf8(body).unwrap()
}

#[test]
fn two_hosts_on_one_machine_each_list_only_their_own_agents() {
    let mut machine = Machine::new("two");
    machine.agent("beta", "b1");
    machine.agent("prod", "p1");
    machine.agent("prod", "p2");
    let mut prod = machine.beat(&[("AGENT_HOST_TMUX_SOCKET", "prod")]);
    prod.sort();
    assert_eq!(prod, ["p1", "p2"]);
    let home = machine.dir.join("home");
    let config = home.join(".config/agent-host/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "tmux_socket = \"beta\"\n").unwrap();
    assert_eq!(machine.beat(&[]), ["b1"]);
}

#[test]
fn a_host_upgraded_with_no_new_setting_finds_the_agents_it_already_runs() {
    let mut machine = Machine::new("upgrade");
    // Where every host started its agents before the socket was a setting.
    machine.agent("agent-host", "a1");
    machine.agent("elsewhere", "x1");
    assert_eq!(machine.beat(&[]), ["a1"]);
}
