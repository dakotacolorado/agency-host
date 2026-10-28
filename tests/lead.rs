//! A lead staffing its pod, end to end: this binary beats to the beta service,
//! a person places a lead on it, and the lead -- Claude Code itself, with only
//! the standing rules and its credential -- places one agent under itself.
//! It costs real sessions and a beta host, so it is run by hand:
//! `cargo test -- --ignored real_claude`. The service is `FLEET_BETA_URL` and
//! `FLEET_BETA_TOKEN`, or `~/.fleet-beta-url` and `~/.fleet-beta-token`.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

fn setting(var: &str, file: &str) -> String {
    env::var(var)
        .ok()
        .or_else(|| fs::read_to_string(PathBuf::from(env::var("HOME").unwrap()).join(file)).ok())
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .unwrap_or_else(|| panic!("set {var} or ~/{file}"))
}

struct Fleet {
    url: String,
    authorization: String,
}

impl Fleet {
    fn call(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let request = ureq::request(method, &format!("{}{path}", self.url))
            .set("Authorization", &self.authorization);
        let response = match body {
            Some(body) => request.send_json(body),
            None => request.call(),
        };
        match response {
            Ok(r) => r.into_json().unwrap_or(Value::Null),
            Err(ureq::Error::Status(code, r)) => {
                panic!(
                    "{method} {path}: {code} {}",
                    r.into_string().unwrap_or_default()
                )
            }
            Err(e) => panic!("{method} {path}: {e}"),
        }
    }

    fn agents(&self, host: &str) -> Vec<Value> {
        self.call("GET", &format!("/fleet/agents?hostId={host}"), None)["agents"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }
}

/// The host, its tmux server and everything placed on it, ended however the
/// test does.
struct Host<'a> {
    fleet: &'a Fleet,
    id: String,
    dir: PathBuf,
    child: Child,
}

impl Drop for Host<'_> {
    fn drop(&mut self) {
        for agent in self.fleet.agents(&self.id) {
            if agent["active"] == true {
                let id = agent["id"].as_str().unwrap();
                let _ = ureq::delete(&format!("{}/fleet/agents/{id}?force=true", self.fleet.url))
                    .set("Authorization", &self.fleet.authorization)
                    .call();
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = Command::new("tmux")
            .env_remove("TMUX")
            .env("TMUX_TMPDIR", self.dir.join("tmux"))
            .args(["-L", "agent-host", "kill-server"])
            .output();
    }
}

#[test]
#[ignore]
fn real_claude_lead_places_one_agent_under_itself_on_beta() {
    let fleet = Fleet {
        url: setting("FLEET_BETA_URL", ".fleet-beta-url"),
        authorization: format!(
            "Bearer {}",
            setting("FLEET_BETA_TOKEN", ".fleet-beta-token")
        ),
    };
    let id = format!("real-lead-{}", std::process::id());
    // Short, since a unix socket's path is limited to about a hundred bytes.
    let dir = env::temp_dir().join(format!("ah-{}-lead", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("tmux")).unwrap();
    // The person's own HOME, where Claude Code is signed in; everything of
    // agent-host's, and the tmux server, under the test's own folder.
    let child = Command::new(env!("CARGO_BIN_EXE_agent-host"))
        .env_remove("TMUX")
        .env_remove("AGENT_HOST_CONFIG")
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("TMUX_TMPDIR", dir.join("tmux"))
        .env("AGENT_HOST_SERVICE_URL", &fleet.url)
        .env(
            "AGENT_HOST_TOKEN",
            setting("FLEET_BETA_TOKEN", ".fleet-beta-token"),
        )
        .env("AGENT_HOST_ID", &id)
        .env("AGENT_HOST_CAPACITY", "2")
        .env("AGENT_HOST_LABELS", "")
        .env("AGENT_HOST_REPOSITORIES", "")
        .env("AGENT_HOST_TMUX_SOCKET", "agent-host")
        .spawn()
        .unwrap();
    let host = Host {
        fleet: &fleet,
        id: id.clone(),
        dir,
        child,
    };

    let deadline = Instant::now() + Duration::from_secs(60);
    while !fleet.call("GET", "/fleet/hosts", None)["hosts"]
        .as_array()
        .is_some_and(|hosts| hosts.iter().any(|h| h["id"] == id.as_str()))
    {
        assert!(Instant::now() < deadline, "host {id} never beat");
        thread::sleep(Duration::from_secs(2));
    }

    let lead = fleet.call(
        "POST",
        "/fleet/agents",
        Some(json!({
            "hostId": id,
            "ticketId": null,
            "role": "lead",
            "createdBy": null,
            "brief": format!(
                "This is a test of placing an agent, and the whole of your work. Your ticket is \
                 none, so the agent you place has none either. Place exactly one agent, as \
                 yourself, as your rules show: hostId \"{id}\", ticketId null, role \"worker\", \
                 no repository, and the brief \"Reply with one word: ready.\" Then reply with \
                 one line: the id the fleet gave it. Do nothing else."
            ),
        })),
    );
    let lead_id = lead["id"].as_str().unwrap().to_string();
    assert_eq!(lead["level"], 0, "a person placed it");

    let deadline = Instant::now() + Duration::from_secs(300);
    let placed = loop {
        assert!(
            Instant::now() < deadline,
            "the lead placed no agent; its transcript: {}",
            fleet.call("GET", &format!("/fleet/agents/{lead_id}/transcript"), None)
        );
        if let Some(agent) = fleet
            .agents(&id)
            .into_iter()
            .find(|a| a["createdBy"] == lead_id.as_str())
        {
            break agent;
        }
        thread::sleep(Duration::from_secs(5));
    };
    assert_eq!(
        placed["level"], 1,
        "its level follows from the lead's: {placed}"
    );
    assert_eq!(placed["role"], "worker", "{placed}");
    assert_eq!(placed["hostId"], id.as_str(), "{placed}");
    assert_eq!(
        fleet.agents(&id).len(),
        2,
        "one lead and the one agent it placed"
    );

    let transcript = fleet.call("GET", &format!("/fleet/agents/{lead_id}/transcript"), None);
    eprintln!("{transcript:#}");
    assert!(
        !transcript.to_string().contains(&format!("agt_{lead_id}_")),
        "the lead's credential is in its transcript"
    );
    drop(host);
}
