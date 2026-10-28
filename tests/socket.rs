//! The built binary, running on, takes a command pushed on its socket at once
//! and reports on the socket as soon as what it did shows: well inside the 30
//! seconds between heartbeats. A socket that drops is opened again and told
//! every worker's state, and a command the service pushes again on it is
//! answered, not done again.

mod common;

use common::{push, report_until, result, Service};
use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// Answers its opening message, and ends its turn.
const CLAUDE: &str = r#"#!/bin/sh
dir="$CLAUDE_CONFIG_DIR/projects/-fake"; mkdir -p "$dir"; t="$dir/$3.jsonl"
printf '%s\n' "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"$4\"}}" >> "$t"
printf '%s\n' '{"type":"assistant","message":{"id":"m0","content":[{"type":"text","text":"ready"}],"stop_reason":"end_turn"}}' >> "$t"
sleep 60
"#;

const TMUX: &str = "wh-test-sock";

struct Machine {
    dir: PathBuf,
    child: Child,
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = Command::new("tmux")
            .env_remove("TMUX")
            .env("TMUX_TMPDIR", self.dir.join("tmux"))
            .args(["-L", TMUX, "kill-server"])
            .output();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn a_command_pushed_on_the_socket_is_taken_and_reported_at_once() {
    // Short, since a unix socket's path is limited to about a hundred bytes.
    let dir = std::env::temp_dir().join(format!("wh-{}-sock", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    for sub in ["tmux", "bin", "home"] {
        fs::create_dir_all(dir.join(sub)).unwrap();
    }
    fs::write(dir.join("bin/claude"), CLAUDE).unwrap();
    fs::set_permissions(dir.join("bin/claude"), fs::Permissions::from_mode(0o755)).unwrap();

    let service = Service::start();
    let child = common::host(
        &dir,
        &format!(
            "service_url = \"{}\"\ntoken = \"the-hosts-own\"\nhost_id = \"studio\"\ntmux_socket = \"{TMUX}\"\n",
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
        json!([{"id":"w1","state":"idle","sessionId":null}])
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
        waiting |= report["workers"][0]["state"] == "waiting";
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
        json!([{"id":"w1","state":"idle","sessionId":null}])
    );

    // Dropped without a close, as a network that goes away does. Nothing has
    // changed, yet the socket opened again is told every worker's state.
    drop(socket);
    let (mut socket, _, _) = service.accept();
    let reopened = report_until(&mut socket, quick, |_| true);
    assert_eq!(
        reopened["workers"],
        json!([{"id":"w1","state":"idle","sessionId":null}])
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
