//! scripts/heartbeat-cycle.sh must skip, not fail, when there is no beta
//! service to send heartbeats to: CI went red once because it ran against a
//! service not built yet.

use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;

struct Run {
    ok: bool,
    output: String,
    proved: String,
}

fn heartbeat_cycle(name: &str, service_url: &str) -> Run {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    fs::create_dir_all(&dir).unwrap();
    let github_output = dir.join("github-output");
    fs::write(&github_output, "").unwrap();
    let out = Command::new("sh")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scripts/heartbeat-cycle.sh"
        ))
        .arg(env!("CARGO_BIN_EXE_worker-host"))
        .env("WORKER_HOST_SERVICE_URL", service_url)
        .env("WORKER_HOST_TOKEN", "unused")
        .env("WORKER_HOST_ID", "test")
        .env("GITHUB_OUTPUT", &github_output)
        .output()
        .unwrap();
    Run {
        ok: out.status.success(),
        output: String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr),
        proved: fs::read_to_string(&github_output).unwrap(),
    }
}

/// A port that was free a moment ago, so nothing answers on it.
fn silent_url() -> String {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    format!("http://127.0.0.1:{port}")
}

#[test]
fn skips_when_given_no_service() {
    let run = heartbeat_cycle("no-service", "");
    assert!(run.ok, "{}", run.output);
    assert_eq!(run.proved, "proved=false\n");
    assert!(
        run.output.contains("waiting for a beta service url"),
        "{}",
        run.output
    );
}

#[test]
fn skips_when_the_service_does_not_answer() {
    let url = silent_url();
    let run = heartbeat_cycle("silent-service", &url);
    assert!(run.ok, "{}", run.output);
    assert_eq!(run.proved, "proved=false\n");
    assert!(
        run.output
            .contains(&format!("waiting for the beta service at {url} to answer")),
        "{}",
        run.output
    );
}
