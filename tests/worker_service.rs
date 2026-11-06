//! The built binary against a deployed worker-service, through the routes a
//! person uses: a worker created with a brief on this host is created and
//! started here, the account's usage its session says is kept, a message
//! reaches it, an interrupt and a stop are taken, and it is retired -- each heard over the socket well inside one heartbeat. The
//! worker's Claude Code is a stand-in, so this costs no session. It runs only
//! with `WORKER_SERVICE_URL` and `WORKER_SERVICE_TOKEN` (beta's `ApiUrl` and
//! token), and otherwise says so and passes.

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

    fn worker(&self, id: &str) -> Value {
        let got = self.call("GET", &format!("/fleet/workers/{id}"), None);
        got.get("worker").cloned().unwrap_or(got)
    }

    fn usage(&self) -> Value {
        let got = self.call("GET", "/fleet/usage", None);
        got.get("usage").cloned().unwrap_or(got)
    }

    /// Waits until `done` holds of the worker, or fails after `within`.
    fn until(&self, id: &str, within: Duration, what: &str, done: impl Fn(&Value) -> bool) {
        let deadline = Instant::now() + within;
        loop {
            let worker = self.worker(id);
            if done(&worker) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{what} within {within:?}: {worker}"
            );
            thread::sleep(Duration::from_millis(250));
        }
    }

    fn command_done(&self, id: &str, command: &str) -> bool {
        self.call("GET", &format!("/fleet/workers/{id}/commands"), None)["commands"]
            .as_array()
            .is_some_and(|all| {
                all.iter().any(|c| {
                    (c["id"] == command || c["command"]["id"] == command) && c["delivery"] == "done"
                })
            })
    }
}

/// The host and the worker made on it, ended however the test does.
struct Host<'a> {
    service: &'a Service,
    id: String,
    dir: PathBuf,
    child: Child,
    worker: Option<String>,
}

impl Drop for Host<'_> {
    fn drop(&mut self) {
        if let Some(worker) = &self.worker {
            let _ = ureq::delete(&format!("{}/fleet/workers/{worker}", self.service.url))
                .set("Authorization", &self.service.authorization)
                .call();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = ureq::delete(&format!("{}/fleet/hosts/{}", self.service.url, self.id))
            .set("Authorization", &self.service.authorization)
            .call();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn a_worker_is_created_spoken_to_stopped_and_retired_over_the_socket() {
    let (Some(url), Some(token)) = (
        env::var("WORKER_SERVICE_URL")
            .ok()
            .filter(|v| !v.is_empty()),
        env::var("WORKER_SERVICE_TOKEN")
            .ok()
            .filter(|v| !v.is_empty()),
    ) else {
        eprintln!(
            "skipped: set WORKER_SERVICE_URL and WORKER_SERVICE_TOKEN to run against a service"
        );
        return;
    };
    let service = Service {
        url: url.trim_end_matches('/').into(),
        authorization: format!("Bearer {token}"),
    };
    let id = format!("test-{}", std::process::id());
    let dir = env::temp_dir().join(format!("wh-{}-svc", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("home")).unwrap();
    common::install_claude(&dir);
    let path = format!(
        "{}:{}",
        dir.join("bin").display(),
        env::var("PATH").unwrap_or_default()
    );
    // No capacity, so the service places no other worker here: a worker made
    // without a host (document-service's judges, on beta) once landed on this
    // one while it had room, and the test's own create then found it full.
    // A worker named for this host is made and started here all the same.
    let config = format!(
        "service_url = {:?}\ntoken = {token:?}\nhost_id = {id:?}\n",
        service.url
    );
    fs::write(dir.join("config.toml"), config).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_worker-host"))
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
        worker: None,
    };

    let deadline = Instant::now() + Duration::from_secs(60);
    while !service.call("GET", "/fleet/hosts", None)["hosts"]
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
        "/fleet/workers",
        Some(json!({"name": name, "hostId": id, "brief": "hello"})),
    );
    let created = created.get("worker").cloned().unwrap_or(created);
    let worker = created["id"].as_str().unwrap().to_string();
    host.worker = Some(worker.clone());
    service.until(&worker, PUSHED, "created and started by push", |w| {
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
        &format!("/fleet/workers/{worker}/messages"),
        Some(json!({"text": "are you there"})),
    );
    let command = sent["commandId"].as_str().unwrap().to_string();
    let deadline = Instant::now() + PUSHED;
    while !service.command_done(&worker, &command) {
        assert!(
            Instant::now() < deadline,
            "message {command} not done by push"
        );
        thread::sleep(Duration::from_millis(250));
    }
    let transcript = service.call("GET", &format!("/fleet/workers/{worker}/transcript"), None);
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

    service.call(
        "POST",
        &format!("/fleet/workers/{worker}/stop"),
        Some(json!({})),
    );
    service.until(&worker, PUSHED, "stopped by push", |w| w["state"] == "idle");

    service.call("DELETE", &format!("/fleet/workers/{worker}"), None);
    service.until(&worker, PUSHED, "retired by push", |w| {
        w["state"] == "retired"
    });
    assert!(
        host.dir
            .join(format!("home/workers/.retired/{name}-{worker}"))
            .is_dir(),
        "its folder is kept"
    );
    host.worker = None;
}
