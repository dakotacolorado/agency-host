//! `worker-host --version` is how a machine is asked which build it runs,
//! without the service.

use std::process::Command;

fn run(program: &str, args: &[&str]) -> String {
    let out = Command::new(program)
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{program} {args:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[test]
fn names_the_commit_or_says_it_cannot() {
    let version = run(env!("CARGO_BIN_EXE_worker-host"), &["--version"]);
    // The commit is the last word on the line, because CI asserts the same
    // shape with `grep -qx "worker-host .* $GITHUB_SHA"`.
    let commit = version
        .strip_prefix(concat!("worker-host ", env!("CARGO_PKG_VERSION"), " "))
        .and_then(|rest| rest.split_whitespace().last())
        .unwrap_or_else(|| panic!("{version}"));
    if commit != "dirty" && commit != "unknown" {
        assert_eq!(commit, run("git", &["rev-parse", "HEAD"]));
        assert_eq!(
            run("git", &["status", "--porcelain", "--untracked-files=no"]),
            "",
            "a build from changed files must not name a commit"
        );
    }
}
