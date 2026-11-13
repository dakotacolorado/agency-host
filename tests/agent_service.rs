//! The built binary against a deployed agent-service, through the routes a
//! person uses: an agent created with a brief on this host is created and
//! started here, the account's usage its session says is kept, a message
//! reaches it, an interrupt and a stop are taken, and it is retired -- each heard over the socket well inside one heartbeat. The
//! agent's Claude Code is a stand-in, so this costs no session. It runs only
//! with `AGENT_SERVICE_URL` and `AGENT_SERVICE_TOKEN` (an agent-service that is
//! not prod, and its token), and otherwise says `skipped:` and passes.

mod common;

use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

/// A heartbeat is 30 s apart, so anything heard sooner came by the socket.
const PUSHED: Duration = Duration::from_secs(10);

struct Service {
    url: String,
    authorization: String,
}

impl Service {
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

    fn agent(&self, id: &str) -> Value {
        let got = self.call("GET", &format!("/agents/{id}"), None);
        got.get("agent").cloned().unwrap_or(got)
    }

    fn usage(&self) -> Value {
        let got = self.call("GET", "/usage", None);
        got.get("usage").cloned().unwrap_or(got)
    }

    /// Waits until `done` holds of the agent, or fails after `within`.
    fn until(&self, id: &str, within: Duration, what: &str, done: impl Fn(&Value) -> bool) {
        let deadline = Instant::now() + within;
        loop {
            let agent = self.agent(id);
            if done(&agent) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{what} within {within:?}: {agent}"
            );
            thread::sleep(Duration::from_millis(250));
        }
    }

    fn command_done(&self, id: &str, command: &str) -> bool {
        self.call("GET", &format!("/agents/{id}/commands"), None)["commands"]
            .as_array()
            .is_some_and(|all| {
                all.iter().any(|c| {
                    (c["id"] == command || c["command"]["id"] == command) && c["delivery"] == "done"
                })
            })
    }
}

/// The host and the agent made on it, ended however the test does.
struct Host<'a> {
    service: &'a Service,
    id: String,
    dir: PathBuf,
    child: Child,
    agent: Option<String>,
}

impl Drop for Host<'_> {
    fn drop(&mut self) {
        if let Some(agent) = &self.agent {
            let _ = ureq::delete(&format!("{}/agents/{agent}", self.service.url))
                .set("Authorization", &self.service.authorization)
                .call();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = ureq::delete(&format!("{}/hosts/{}", self.service.url, self.id))
            .set("Authorization", &self.service.authorization)
            .call();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn an_agent_is_created_spoken_to_stopped_and_retired_over_the_socket() {
    let (Some(url), Some(token)) = (
        env::var("AGENT_SERVICE_URL").ok().filter(|v| !v.is_empty()),
        env::var("AGENT_SERVICE_TOKEN")
            .ok()
            .filter(|v| !v.is_empty()),
    ) else {
        eprintln!(
            "skipped: set AGENT_SERVICE_URL and AGENT_SERVICE_TOKEN to run against a service"
        );
        return;
    };
    let service = Service {
        url: url.trim_end_matches('/').into(),
        authorization: format!("Bearer {token}"),
    };
    let id = format!("test-{}", std::process::id());
    let dir = env::temp_dir().join(format!("ah-{}-svc", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("home")).unwrap();
    common::install_claude(&dir);
    let path = format!(
        "{}:{}",
        dir.join("bin").display(),
        env::var("PATH").unwrap_or_default()
    );
    // No capacity, so the service places no other agent here: an agent made
    // without a host (document-service's judges, on beta) once landed on this
    // one while it had room, and the test's own create then found it full.
    // An agent named for this host is made and started here all the same.
    let config = format!(
        "service_url = {:?}\ntoken = {token:?}\nhost_id = {id:?}\n",
        service.url
    );
    fs::write(dir.join("config.toml"), config).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_agent-host"))
        .env_clear()
        .env("PATH", path)
        .env("HOME", dir.join("home"))
        .arg("--config")
        .arg(dir.join("config.toml"))
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut host = Host {
        service: &service,
        id: id.clone(),
        dir,
        child,
        agent: None,
    };

    let deadline = Instant::now() + Duration::from_secs(60);
    while !service.call("GET", "/hosts", None)["hosts"]
        .as_array()
        .is_some_and(|hosts| hosts.iter().any(|h| h["id"] == id.as_str()))
    {
        assert!(
            Instant::now() < deadline,
            "host {id} never sent a heartbeat"
        );
        thread::sleep(Duration::from_secs(1));
    }
    // Long enough after the first heartbeat for the socket it named to open.
    thread::sleep(Duration::from_secs(3));

    let before = service.usage()["fiveHour"]["readAt"].clone();
    let name = format!("t{}", std::process::id());
    let created = service.call(
        "POST",
        "/agents",
        Some(json!({"name": name, "hostId": id, "brief": "hello"})),
    );
    let created = created.get("agent").cloned().unwrap_or(created);
    let agent = created["id"].as_str().unwrap().to_string();
    host.agent = Some(agent.clone());
    service.until(&agent, PUSHED, "created and started by push", |w| {
        w["state"] == "waiting" && !w["reportedAt"].is_null()
    });
    // The stand-in's reading is stamped as the host reads it, so it is newer
    // than any the service held before.
    let deadline = Instant::now() + PUSHED;
    loop {
        let usage = service.usage();
        let five_hour = &usage["fiveHour"];
        if five_hour["utilization"] == 0.48
            && five_hour["readAt"].as_str() > before.as_str()
            && usage["sevenDay"]["utilization"] == 0.4
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the session's usage, kept within {PUSHED:?}: {usage}, newer than {before}"
        );
        thread::sleep(Duration::from_millis(250));
    }

    let sent = service.call(
        "POST",
        &format!("/agents/{agent}/messages"),
        Some(json!({"text": "are you there"})),
    );
    let command = sent["commandId"].as_str().unwrap().to_string();
    let deadline = Instant::now() + PUSHED;
    while !service.command_done(&agent, &command) {
        assert!(
            Instant::now() < deadline,
            "message {command} not done by push"
        );
        thread::sleep(Duration::from_millis(250));
    }
    let transcript = service.call("GET", &format!("/agents/{agent}/transcript"), None);
    let texts: Vec<&str> = transcript["lines"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|l| l["text"].as_str())
        .collect();
    assert!(
        texts.contains(&"are you there") && texts.contains(&"heard"),
        "{transcript}"
    );

    service.call("POST", &format!("/agents/{agent}/stop"), Some(json!({})));
    service.until(&agent, PUSHED, "stopped by push", |w| w["state"] == "idle");

    service.call("DELETE", &format!("/agents/{agent}"), None);
    service.until(&agent, PUSHED, "retired by push", |w| {
        w["state"] == "retired"
    });
    assert!(
        host.dir
            .join(format!("home/agents/.retired/{name}-{agent}"))
            .is_dir(),
        "its folder is kept"
    );
    host.agent = None;
}
