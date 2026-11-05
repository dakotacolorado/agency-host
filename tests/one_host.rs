//! One host to a workers folder: a second, started while the first runs, is
//! refused rather than taking every command a second time.

mod common;

use std::fs;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn a_second_host_on_the_same_workers_folder_is_refused() {
    let dir = std::env::temp_dir().join(format!("wh-t-one-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("home")).unwrap();
    let service = common::Service::start();
    let workers = dir.join("workers");
    let config = format!(
        "service_url = \"{}\"\ntoken = \"t\"\nhost_id = \"studio\"\ncapacity = 1\nworkers_dir = \"{}\"\n",
        service.url,
        workers.display()
    );
    let mut first = common::host(&dir, &config);
    let lock = workers.join(".worker-host.lock");
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

    let second = Command::new(env!("CARGO_BIN_EXE_worker-host"))
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
        String::from_utf8_lossy(&second.stderr).contains("another worker-host is already running"),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
}
