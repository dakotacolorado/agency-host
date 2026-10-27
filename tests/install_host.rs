//! scripts/install-host.sh must refuse what it cannot stand behind before it
//! changes anything on the machine.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Run {
    ok: bool,
    output: String,
    home: PathBuf,
}

fn install_host(name: &str, args: &[&str], service_url: &str, env: &[(&str, &str)]) -> Run {
    let home = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(&home).unwrap();
    let out = Command::new("sh")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/scripts/install-host.sh"
        ))
        .args(args)
        .env("HOME", &home)
        .env_remove("XDG_CONFIG_HOME")
        .env("AGENT_HOST_SERVICE_URL", service_url)
        .env("AGENT_HOST_TOKEN", "secret-token")
        .envs(env.iter().copied())
        .output()
        .unwrap();
    Run {
        ok: out.status.success(),
        output: String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr),
        home,
    }
}

fn is_empty(dir: &Path) -> bool {
    fs::read_dir(dir).unwrap().next().is_none()
}

#[test]
fn refuses_a_fleet_api_not_over_https() {
    let run = install_host("plain-http", &[], "http://fleet.example", &[]);
    assert!(!run.ok, "{}", run.output);
    assert!(run.output.contains("over https only"), "{}", run.output);
    assert!(!run.output.contains("secret-token"), "{}", run.output);
    assert!(is_empty(&run.home), "wrote into {}", run.home.display());
}

#[test]
fn has_nothing_to_roll_back_to_before_a_second_install() {
    let run = install_host("no-previous", &["rollback"], "https://fleet.example", &[]);
    assert!(!run.ok, "{}", run.output);
    assert!(
        run.output.contains("nothing to roll back to"),
        "{}",
        run.output
    );
    assert!(is_empty(&run.home), "wrote into {}", run.home.display());
}

#[test]
fn refuses_a_capacity_that_is_not_a_number() {
    let run = install_host(
        "capacity-not-a-number",
        &[],
        "https://fleet.example",
        &[("AGENT_HOST_CAPACITY", "lots")],
    );
    assert!(!run.ok, "{}", run.output);
    assert!(run.output.contains("not 'lots'"), "{}", run.output);
    assert!(is_empty(&run.home), "wrote into {}", run.home.display());
}

#[test]
fn refuses_a_repository_that_is_not_an_absolute_path() {
    let run = install_host(
        "repository-relative",
        &[],
        "https://fleet.example",
        &[(
            "AGENT_HOST_REPOSITORIES",
            "widgets=/src/widgets,gadgets=src/gadgets",
        )],
    );
    assert!(!run.ok, "{}", run.output);
    assert!(run.output.contains("name=/absolute/path"), "{}", run.output);
    assert!(is_empty(&run.home), "wrote into {}", run.home.display());
}
