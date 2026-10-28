//! What build.rs stamps for each kind of tree a build can start from. These
//! run its own `stamp` against throwaway repos, since only the tree this suite
//! was built from can be checked through the binary.

#[allow(dead_code)]
#[path = "../build.rs"]
mod build;

use std::{fs, path::PathBuf, process::Command};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("worker-host-stamp-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn git(dir: &PathBuf, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn crate_in(dir: &PathBuf) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("Cargo.toml"), "[package]\n").unwrap();
    fs::write(dir.join("build.rs"), "fn main() {}\n").unwrap();
}

#[test]
fn a_clean_tree_names_its_commit_and_a_changed_one_says_dirty() {
    let repo = scratch("tracked");
    git(&repo, &["init", "-q"]);
    crate_in(&repo);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "c"]);
    assert_eq!(build::stamp(&repo), git(&repo, &["rev-parse", "HEAD"]));

    fs::write(repo.join("build.rs"), "fn main() { }\n").unwrap();
    assert_eq!(build::stamp(&repo), "dirty");
    fs::remove_dir_all(&repo).unwrap();
}

#[test]
fn source_untracked_inside_an_unrelated_repo_is_unknown() {
    let outer = scratch("vendored");
    git(&outer, &["init", "-q"]);
    fs::write(outer.join("unrelated"), "").unwrap();
    git(&outer, &["add", "."]);
    git(&outer, &["commit", "-qm", "c"]);
    let vendored = outer.join("vendor/worker-host");
    crate_in(&vendored);
    assert_eq!(build::stamp(&vendored), "unknown");
    fs::remove_dir_all(&outer).unwrap();
}

#[test]
fn source_outside_any_repo_is_unknown() {
    let dir = scratch("bare");
    crate_in(&dir);
    assert_eq!(build::stamp(&dir), "unknown");
    fs::remove_dir_all(&dir).unwrap();
}
