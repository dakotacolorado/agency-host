//! One host to an agents folder: a second, started while the first runs, is
//! refused rather than taking every command a second time.

mod common;

use std::fs;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn a_second_host_on_the_same_agents_folder_is_refused() {
    let dir = std::env::temp_dir().join(format!("ah-t-one-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("home")).unwrap();
    let service = common::Service::start();
    let agents = dir.join("agents");
    let config = format!(
        "service_url = \"{}\"\ntoken = \"t\"\nhost_id = \"studio\"\ncapacity = 1\nagents_dir = \"{}\"\n",
        service.url,
        agents.display()
    );
    let mut first = common::host(&dir, &config);
    let lock = agents.join(".agent-host.lock");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !lock.exists() {
        assert!(
            Instant::now() < deadline,
            "the first host never took the lock"
        );
        thread::sleep(Duration::from_millis(50));
    }
    // The file is made a moment before it is locked.
    thread::sleep(Duration::from_millis(300));

    let second = Command::new(env!("CARGO_BIN_EXE_agent-host"))
        .env_clear()
        .env("HOME", dir.join("home"))
        .arg("--config")
        .arg(dir.join("config.toml"))
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    first.kill().unwrap();
    let _ = first.wait();
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(second.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("another agent-host is already running"),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
}
