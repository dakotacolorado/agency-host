//! Two hosts on one machine each report only their own agents, which needs
//! each on an agents folder of its own. A session a host finds live there is
//! resumed, and one that was stopped is left.

mod common;

use common::{report_until, Service};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

struct Machine {
    dir: PathBuf,
}

impl Machine {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("ah-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("home")).unwrap();
        common::install_claude(&dir);
        Machine { dir }
    }

    /// An agent's folder as a host keeps one, with one session begun, and
    /// stopped if `stopped`.
    fn agent(&self, agents: &Path, id: &str, name: &str, stopped: bool) {
        let home = agents.join(name);
        fs::create_dir_all(&home).unwrap();
        let record = json!({"id": id, "name": name});
        fs::write(home.join("agent.json"), record.to_string()).unwrap();
        let session = format!("s-{id}");
        let mut lines = json!({"sessionId": session, "claude": "c", "from": 0, "stream": true})
            .to_string()
            + "\n";
        if stopped {
            lines += &(json!({"sessionId": session, "stopped": true}).to_string() + "\n");
        }
        fs::write(home.join("sessions.jsonl"), lines).unwrap();
    }

    /// The agents a host tells its socket of once it has taken up what it
    /// found, as (id, state), `live` for a session working or waiting, run
    /// with `settings` and nothing from the person's own config or folders.
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
        report["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| {
                let state = match w["state"].as_str().unwrap() {
                    "working" | "waiting" => "live",
                    other => other,
                };
                (w["id"].as_str().unwrap().to_string(), state.to_string())
            })
            .collect()
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

#[test]
fn two_hosts_on_one_machine_each_report_only_their_own_agents() {
    let machine = Machine::new("two");
    let (beta, prod) = (machine.dir.join("beta"), machine.dir.join("prod"));
    machine.agent(&beta, "b1", "bea", false);
    machine.agent(&prod, "p1", "pat", false);
    machine.agent(&prod, "p2", "pia", true);
    assert_eq!(
        machine.opening(&format!("agents_dir = \"{}\"\n", prod.display())),
        pairs(&[("p1", "live"), ("p2", "idle")]),
        "p1 was live, and is resumed; p2 was stopped"
    );
    assert_eq!(
        machine.opening(&format!("agents_dir = \"{}\"\n", beta.display())),
        pairs(&[("b1", "live")])
    );
}

#[test]
fn a_host_with_no_agents_dir_finds_its_agents_in_home() {
    let machine = Machine::new("defaults");
    machine.agent(&machine.dir.join("home/agents"), "w1", "ana", false);
    machine.agent(&machine.dir.join("home/agents"), "w2", "bo", true);
    assert_eq!(
        machine.opening(""),
        pairs(&[("w1", "live"), ("w2", "idle")])
    );
}
