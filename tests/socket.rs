//! The built binary, running on, takes a command pushed on its socket at once
//! and reports on the socket as soon as what it did shows: well inside the 30
//! seconds between heartbeats. A socket that drops is opened again and told
//! every worker's state, and a command the service pushes again on it is
//! answered, not done again.

mod common;

use common::{push, report_until, result, Service};
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::process::Child;
use std::time::{Duration, Instant};

struct Machine {
    dir: PathBuf,
    child: Child,
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn a_command_pushed_on_the_socket_is_taken_and_reported_at_once() {
    let dir = std::env::temp_dir().join(format!("wh-{}-sock", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("home")).unwrap();
    common::install_claude(&dir);

    let service = Service::start();
    let child = common::host(
        &dir,
        &format!(
            "service_url = \"{}\"\ntoken = \"the-hosts-own\"\nhost_id = \"studio\"\n",
            service.url
        ),
    );
    let _machine = Machine { dir, child };

    let (mut socket, path, authorization) = service.accept();
    assert_eq!(path, "/beta?hostId=studio");
    assert_eq!(authorization, "Bearer the-hosts-own");
    let opening = report_until(&mut socket, Duration::from_secs(5), |_| true);
    assert_eq!(opening["workers"], json!([]), "a whole report on opening");

    let quick = Duration::from_secs(10);
    push(
        &mut socket,
        json!({"id":"c1","kind":"create","workerId":"w1","name":"ana"}),
    );
    let created = report_until(&mut socket, quick, |r| result(r, "c1").is_some());
    assert_eq!(result(&created, "c1").unwrap()["ok"], true, "{created}");
    assert_eq!(
        created["workers"],
        json!([{"id":"w1","state":"idle","sessionId":null,"tasks":0}]),
        "no turn yet, so nothing said of one"
    );

    let start = json!({"id":"c2","kind":"start","workerId":"w1","sessionId":"5a1e2b3c-4d5e-4f60-8a7b-8c9d0e1f2a3b","resume":false,"text":"hello"});
    push(&mut socket, start.clone());
    // Its result, its turn ending and what it said may come in one report
    // or in several.
    let (mut ok, mut waiting, mut lines) = (None, false, vec![]);
    let deadline = Instant::now() + quick;
    while ok.is_none() || !waiting || lines.len() < 2 {
        assert!(Instant::now() < deadline, "{ok:?} {waiting} {lines:?}");
        let report = report_until(&mut socket, quick, |_| true);
        ok = ok.or(result(&report, "c2").map(|r| r["ok"].clone()));
        let worker = &report["workers"][0];
        if worker["state"] == "waiting" {
            assert_eq!(
                (&worker["tasks"], &worker["told"]),
                (&json!(0), &json!(true)),
                "its turn was begun by the message: {report}"
            );
            waiting = true;
        }
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
        json!([{"id":"w1","state":"idle","sessionId":null,"tasks":0,"told":true}])
    );

    // Dropped without a close, as a network that goes away does. Nothing has
    // changed, yet the socket opened again is told every worker's state.
    drop(socket);
    let (mut socket, _, _) = service.accept();
    let reopened = report_until(&mut socket, quick, |_| true);
    assert_eq!(
        reopened["workers"],
        json!([{"id":"w1","state":"idle","sessionId":null,"tasks":0,"told":true}])
    );
    assert_eq!(
        reopened["lines"],
        json!([]),
        "lines sent are not sent again"
    );

    // Pushed again as pending, as the service does when a socket opens, the
    // start is answered as before and not done again.
    push(&mut socket, start);
    let again = report_until(&mut socket, quick, |r| result(r, "c2").is_some());
    assert_eq!(result(&again, "c2").unwrap()["ok"], true, "{again}");
    assert_eq!(again["workers"][0]["state"], "idle", "{again}");
}
