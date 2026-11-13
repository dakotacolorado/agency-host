//! The built binary, running on, takes a command pushed on its socket at once
//! and reports on the socket as soon as what it did shows: well inside the 30
//! seconds between heartbeats. A socket that drops is opened again and told
//! every agent's state and every session's tokens, and a command the
//! service pushes again on it is answered, not done again. A host that
//! restarts tells the same totals again.

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
    let dir = std::env::temp_dir().join(format!("ah-{}-sock", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("home")).unwrap();
    common::install_claude(&dir);

    let service = Service::start();
    let config = format!(
        "service_url = \"{}\"\ntoken = \"the-hosts-own\"\nhost_id = \"studio\"\n",
        service.url
    );
    let child = common::host(&dir, &config);
    let mut machine = Machine { dir, child };

    let (mut socket, path, authorization) = service.accept();
    assert_eq!(path, "/beta?hostId=studio");
    assert_eq!(authorization, "Bearer the-hosts-own");
    let opening = report_until(&mut socket, Duration::from_secs(5), |_| true);
    assert_eq!(opening["agents"], json!([]), "a whole report on opening");

    let quick = Duration::from_secs(10);
    push(
        &mut socket,
        json!({"id":"c1","kind":"create","agentId":"w1","name":"ana"}),
    );
    let created = report_until(&mut socket, quick, |r| result(r, "c1").is_some());
    assert_eq!(result(&created, "c1").unwrap()["ok"], true, "{created}");
    assert_eq!(
        created["agents"],
        json!([{"id":"w1","state":"idle","sessionId":null,"tasks":0}]),
        "no turn yet, so nothing said of one"
    );

    // Its id is a uuid, as the service's are, so its text's line names it.
    let c2 = "6b2f3c4d-5e6f-4a70-8b8c-9d0e1f2a3b4c";
    let start = json!({"id":c2,"kind":"start","agentId":"w1","sessionId":"5a1e2b3c-4d5e-4f60-8a7b-8c9d0e1f2a3b","resume":false,"text":"hello"});
    push(&mut socket, start.clone());
    // Its result, its turn ending, the account's usage and what it said may
    // come in one report or in several.
    let (mut ok, mut waiting, mut lines, mut usage) = (None, false, vec![], None);
    let deadline = Instant::now() + quick;
    while ok.is_none() || !waiting || lines.len() < 2 || usage.is_none() {
        assert!(
            Instant::now() < deadline,
            "{ok:?} {waiting} {lines:?} {usage:?}"
        );
        let report = report_until(&mut socket, quick, |_| true);
        ok = ok.or(result(&report, c2).map(|r| r["ok"].clone()));
        usage = usage.or(report.get("usage").cloned());
        let agent = &report["agents"][0];
        if agent["state"] == "waiting" {
            assert_eq!(
                (&agent["tasks"], &agent["told"]),
                (&json!(0), &json!(true)),
                "its turn was begun by the message: {report}"
            );
            waiting = true;
        }
        lines.extend(report["lines"].as_array().cloned().unwrap_or_default());
    }
    assert_eq!(ok, Some(json!(true)));
    let usage = usage.unwrap();
    assert_eq!(
        usage,
        json!({
            "readAt": usage["readAt"],
            "fiveHour": {"utilization": 0.48, "resetsAt": "2026-11-06T11:40:00.000Z"},
            "sevenDay": {"utilization": 0.4, "resetsAt": "2026-11-07T15:00:00.000Z"},
        })
    );
    assert!(
        usage["readAt"].as_str().is_some_and(|at| at.ends_with('Z')),
        "{usage}"
    );
    assert_eq!(
        lines,
        [
            json!({"agentId":"w1","seq":0,"at":lines[0]["at"],"sessionId":"5a1e2b3c-4d5e-4f60-8a7b-8c9d0e1f2a3b","from":"instruction","text":"hello","commandId":c2}),
            json!({"agentId":"w1","seq":1,"at":lines[1]["at"],"sessionId":"5a1e2b3c-4d5e-4f60-8a7b-8c9d0e1f2a3b","from":"agent","text":"ready"}),
        ]
    );

    // A message is reported written, not yet taken, and then taken; when the
    // two come between reports, only the last goes up. Its line names it.
    let c4 = "7c3a4d5e-6f70-4b81-9c9d-0e1f2a3b4c5d";
    push(
        &mut socket,
        json!({"id":c4,"kind":"message","agentId":"w1","text":"are you there"}),
    );
    let (mut results, mut lines) = (vec![], vec![]);
    let deadline = Instant::now() + quick;
    while results.last() != Some(&json!({"commandId":c4,"ok":true})) || lines.len() < 2 {
        assert!(Instant::now() < deadline, "{results:?} {lines:?}");
        let report = report_until(&mut socket, quick, |_| true);
        results.extend(result(&report, c4));
        lines.extend(report["lines"].as_array().cloned().unwrap_or_default());
    }
    assert!(
        results[..results.len() - 1] == [json!({"commandId":c4,"ok":true,"taken":false})]
            || results.len() == 1,
        "{results:?}"
    );
    assert_eq!(
        (&lines[0]["text"], &lines[0]["commandId"], &lines[1]["text"]),
        (&json!("are you there"), &json!(c4), &json!("heard")),
        "{lines:?}"
    );
    assert!(lines[1].get("commandId").is_none(), "{lines:?}");

    push(&mut socket, json!({"id":"c3","kind":"stop","agentId":"w1"}));
    let stopped = report_until(&mut socket, quick, |r| result(r, "c3").is_some());
    assert_eq!(result(&stopped, "c3").unwrap()["ok"], true, "{stopped}");
    assert_eq!(
        stopped["agents"],
        json!([{"id":"w1","state":"idle","sessionId":null,"tasks":0,"told":true}])
    );

    // Dropped without a close, as a network that goes away does. Nothing has
    // changed, yet the socket opened again is told every agent's state.
    drop(socket);
    let (mut socket, _, _) = service.accept();
    let reopened = report_until(&mut socket, quick, |_| true);
    assert_eq!(
        reopened["agents"],
        json!([{"id":"w1","state":"idle","sessionId":null,"tasks":0,"told":true}])
    );
    assert_eq!(
        reopened["lines"],
        json!([]),
        "lines sent are not sent again"
    );
    assert!(reopened.get("usage").is_none(), "nor is usage: {reopened}");
    // Two turns, each of which said the same tokens.
    let total = json!([{"agentId":"w1","sessionId":"5a1e2b3c-4d5e-4f60-8a7b-8c9d0e1f2a3b",
        "inputTokens":6,"outputTokens":10,"cacheCreationInputTokens":14,"cacheReadInputTokens":22}]);
    assert_eq!(
        reopened["tokens"], total,
        "but every session's total is: {reopened}"
    );

    // Pushed again as pending, as the service does when a socket opens, the
    // start is answered as before and not done again.
    push(&mut socket, start);
    let again = report_until(&mut socket, quick, |r| result(r, c2).is_some());
    assert_eq!(result(&again, c2).unwrap()["ok"], true, "{again}");
    assert_eq!(again["agents"][0]["state"], "idle", "{again}");

    // The host goes and comes back, and reads the session again from its
    // start to the same total.
    machine.child.kill().unwrap();
    let _ = machine.child.wait();
    drop(socket);
    machine.child = common::host(&machine.dir, &config);
    let (mut socket, _, _) = service.accept();
    let restarted = report_until(&mut socket, quick, |r| r.get("tokens").is_some());
    assert_eq!(restarted["tokens"], total, "{restarted}");
}
