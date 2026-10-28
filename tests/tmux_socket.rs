//! Two hosts on one machine each report only their own workers, which needs
//! each on a tmux server and a workers folder of its own. Every server here is
//! under a TMUX_TMPDIR of the test's, and none is named as a real host's is.

mod common;

use common::{report_until, Service};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

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
        let record = json!({"id": id, "name": name});
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

    /// The workers a host tells its socket of when it opens, as (id,
    /// state), run with `settings` and nothing from the person's own config
    /// or folders.
    fn opening(&self, settings: &str) -> Vec<(String, String)> {
        let service = Service::start();
        let config = format!(
            "service_url = \"{}\"\ntoken = \"unused\"\nhost_id = \"test\"\n{settings}",
            service.url
        );
        let mut child = common::host(&self.dir, &config);
        let (mut socket, _, _) = service.accept();
        let report = report_until(&mut socket, Duration::from_secs(10), |_| true);
        let _ = child.kill();
        let _ = child.wait();
        report["workers"]
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
    machine.session("wh-t-beta", "b1");
    machine.session("wh-t-prod", "p1");
    machine.session("wh-t-beta", "p2");
    assert_eq!(
        machine.opening(&format!(
            "tmux_socket = \"wh-t-prod\"\nworkers_dir = \"{}\"\n",
            prod.display()
        )),
        pairs(&[("p1", "working"), ("p2", "interrupted")]),
        "p2's session is not on prod's server, and was not stopped"
    );
    assert_eq!(
        machine.opening(&format!(
            "tmux_socket = \"wh-t-beta\"\nworkers_dir = \"{}\"\n",
            beta.display()
        )),
        pairs(&[("b1", "working")])
    );
}

#[test]
fn a_host_with_no_workers_dir_finds_its_workers_in_home() {
    let mut machine = Machine::new("defaults");
    machine.worker(&machine.dir.join("home/workers"), "w1", "ana");
    machine.worker(&machine.dir.join("home/workers"), "w2", "bo");
    machine.session("wh-t-home", "w1");
    // On another server: none of this host's.
    machine.session("wh-t-else", "w2");
    assert_eq!(
        machine.opening("tmux_socket = \"wh-t-home\"\n"),
        pairs(&[("w1", "working"), ("w2", "interrupted")])
    );
}
