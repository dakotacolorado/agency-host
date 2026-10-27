//! Stamps the commit this binary is built from into it as AGENT_HOST_COMMIT.
//! Hosts do not upgrade together, so the binary is the only thing that can say
//! which build a machine runs. A tree with uncommitted changes, or no git at
//! all, is stamped `dirty` or `unknown` rather than with a commit it does not
//! match.

use std::{path::Path, process::Command};

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    let commit = match git(&["rev-parse", "--verify", "HEAD"]) {
        None => "unknown".to_string(),
        // Untracked files are left out: one cannot reach the build without a
        // tracked file changing to name it.
        Some(head) => match git(&["status", "--porcelain", "--untracked-files=no"]) {
            Some(changes) if changes.is_empty() => head,
            Some(_) => "dirty".to_string(),
            None => "unknown".to_string(),
        },
    };
    println!("cargo:rustc-env=AGENT_HOST_COMMIT={commit}");

    // By default cargo reruns this only when a package file changes, so a
    // commit that changes nothing on disk would leave `dirty` stamped. Watch
    // what moves HEAD and the index, and every tracked file.
    println!("cargo:rerun-if-changed=build.rs");
    if let (Some(dir), Some(common)) = (
        git(&["rev-parse", "--absolute-git-dir"]),
        git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]),
    ) {
        let head_ref = git(&["symbolic-ref", "-q", "HEAD"]);
        let watched = [
            Some(format!("{dir}/HEAD")),
            Some(format!("{dir}/index")),
            Some(format!("{common}/packed-refs")),
            head_ref.map(|r| format!("{common}/{r}")),
        ];
        // A missing path would make cargo rerun this on every build.
        for path in watched.into_iter().flatten() {
            if Path::new(&path).exists() {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
    for file in git(&["ls-files"]).unwrap_or_default().lines() {
        println!("cargo:rerun-if-changed={file}");
    }
}
