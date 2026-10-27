//! Stamps the commit this binary is built from into it as AGENT_HOST_COMMIT.
//! Hosts do not upgrade together, so the binary is the only thing that can say
//! which build a machine runs. A tree with uncommitted changes, or no git at
//! all, is stamped `dirty` or `unknown` rather than with a commit it does not
//! match.

use std::{path::Path, process::Command};

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    // Which repo holds the source is decided by where the source is, not by a
    // GIT_DIR left in the environment that may name some other repo.
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The commit the crate at `dir` was built from, or `dirty` or `unknown`.
pub fn stamp(dir: &Path) -> String {
    // Source unpacked untracked inside an unrelated repo (a home directory kept
    // in git, a monorepo it was vendored into) would otherwise take that repo's
    // HEAD, which says nothing about this source.
    if git(
        dir,
        &["ls-files", "--error-unmatch", "Cargo.toml", "build.rs"],
    )
    .is_none()
    {
        return "unknown".to_string();
    }
    match git(dir, &["rev-parse", "--verify", "HEAD"]) {
        None => "unknown".to_string(),
        // Untracked files are left out: one cannot reach the build without a
        // tracked file changing to name it.
        Some(head) => match git(dir, &["status", "--porcelain", "--untracked-files=no"]) {
            Some(changes) if changes.is_empty() => head,
            Some(_) => "dirty".to_string(),
            None => "unknown".to_string(),
        },
    }
}

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    println!("cargo:rustc-env=AGENT_HOST_COMMIT={}", stamp(dir));

    // By default cargo reruns this only when a package file changes, so a
    // commit that changes nothing on disk would leave `dirty` stamped. Watch
    // what moves HEAD and the index, and every tracked file.
    println!("cargo:rerun-if-changed=build.rs");
    if let (Some(git_dir), Some(common)) = (
        git(dir, &["rev-parse", "--absolute-git-dir"]),
        git(
            dir,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ),
    ) {
        let head_ref = git(dir, &["symbolic-ref", "-q", "HEAD"]);
        let watched = [
            Some(format!("{git_dir}/HEAD")),
            Some(format!("{git_dir}/index")),
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
    for file in git(dir, &["ls-files"]).unwrap_or_default().lines() {
        println!("cargo:rerun-if-changed={file}");
    }
}
