//! A start too large for its socket message names `contextUrl`: the built
//! binary reads the whole context there and writes it, and one it cannot
//! read starts with what the start carried.

mod common;

use common::{push, report_until, result, Service};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::process::Child;
use std::time::Duration;

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

fn file(path: &str, content: &str) -> Value {
    json!({"path":path,"content":content,"executable":false})
}

/// Starts `ana` with a start carrying only `skills/x/SKILL.md` and naming
/// `contextUrl`, against a service answering it with `whole`, and hands back
/// her Claude Code folder, with the socket, kept open until it is read.
fn started(name: &str, whole: Option<Value>) -> (Machine, PathBuf, common::Socket) {
    let dir = std::env::temp_dir().join(format!("wh-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("home")).unwrap();
    common::install_claude(&dir);
    let service = Service::serving(whole);
    let workers = dir.join("workers");
    let child = common::host(
        &dir,
        &format!(
            "service_url = \"{}\"\ntoken = \"t\"\nhost_id = \"studio\"\nworkers_dir = \"{}\"\n",
            service.url,
            workers.display()
        ),
    );
    let machine = Machine { dir, child };
    let (mut socket, _, _) = service.accept();
    let quick = Duration::from_secs(10);
    push(
        &mut socket,
        json!({"id":"c1","kind":"create","workerId":"w1","name":"ana"}),
    );
    report_until(&mut socket, quick, |r| result(r, "c1").is_some());
    push(
        &mut socket,
        json!({
            "id":"c2","kind":"start","workerId":"w1","sessionId":"5a1e2b3c-4d5e-4f60-8a7b-8c9d0e1f2a3b","resume":false,
            "context":{"key":"global","version":5,"files":[file("skills/x/SKILL.md","Use x.")]},
            "contextUrl":"/fleet/workers/w1/sessions/5a1e2b3c-4d5e-4f60-8a7b-8c9d0e1f2a3b/context",
        }),
    );
    let report = report_until(&mut socket, quick, |r| result(r, "c2").is_some());
    assert_eq!(result(&report, "c2").unwrap()["ok"], true, "{report}");
    let claude = workers.join("ana/home/.claude");
    (machine, claude, socket)
}

#[test]
fn a_start_too_large_to_carry_its_context_is_given_it_whole() {
    let whole = json!({
        "context":{"key":"global","version":5,"files":[file("skills/x/SKILL.md","Use x."),file("skills/x/reference/api.md","The whole API.")]},
        "documents":[{"key":"approval","version":2,"files":[file("skills/approve/SKILL.md","Approve.")]}],
    });
    let (_machine, claude, _socket) = started("whole", Some(whole));
    assert_eq!(
        fs::read_to_string(claude.join("skills/x/reference/api.md")).unwrap(),
        "The whole API."
    );
    assert_eq!(
        fs::read_to_string(claude.join("skills/approve/SKILL.md")).unwrap(),
        "Approve."
    );
}

#[test]
fn a_whole_context_that_cannot_be_read_leaves_what_the_start_carried() {
    let (_machine, claude, _socket) = started("carried", None);
    assert_eq!(
        fs::read_to_string(claude.join("skills/x/SKILL.md")).unwrap(),
        "Use x."
    );
    assert!(!claude.join("skills/x/reference/api.md").exists());
}
