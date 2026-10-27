//! A working tree of its own for each agent that has code to work on: a git
//! worktree of the host's checkout, on the agent's own branch, so two agents
//! on one machine never share a tree, and a person's checkout is never the one
//! an agent works in.
//!
//! A workspace is kept when its agent ends, for whoever wants to see what it
//! left. The worktree is locked, so `git worktree prune` and a plain
//! `git worktree remove` in the checkout leave it alone; removing one is a
//! person's deliberate `git worktree remove --force --force`, never the host's.

use std::fs;
use std::io::Read;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Installed dependency folders a worktree takes from its checkout as a link
/// rather than installing again. A link is shared by every agent given one, so
/// nothing here ever installs into it; what the agent is told about that is in
/// conduct.md.
const SHARED: &[&str] = &["node_modules"];

/// Long enough for a fetch of a large repository; short enough that a network
/// that has hung does not stop the host beating for good.
const GIT_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, PartialEq)]
pub struct Workspace {
    pub dir: PathBuf,
    pub branch: String,
}

/// The branch an agent works on when its placement named none.
pub fn own_branch(id: &str) -> String {
    format!("agent/{id}")
}

/// Makes `dir` a worktree of `checkout` on `branch`, or the agent's own branch
/// when none is named. A named branch is taken from the remote when it is
/// there, since what was pushed to it is the work to carry on from; one that
/// is not becomes new. Anything that would leave the agent without the code it
/// was placed for is an error here, before it starts, rather than something
/// for it to find out.
///
/// A worktree already at `dir` is a start that was cut off before its agent
/// ran, and is taken as it is.
pub fn prepare(
    id: &str,
    checkout: &Path,
    dir: &Path,
    branch: Option<&str>,
) -> Result<Workspace, String> {
    let branch = branch.map_or_else(|| own_branch(id), String::from);
    if dir.exists() {
        let top = git(dir, &["rev-parse", "--show-toplevel"])
            .map_err(|_| format!("{} is there already, and is not a worktree", dir.display()))?;
        if Path::new(&top).canonicalize().ok() != dir.canonicalize().ok() {
            return Err(format!(
                "{} is there already, inside another tree",
                dir.display()
            ));
        }
        link_shared(checkout, dir)?;
        return Ok(Workspace {
            dir: dir.into(),
            branch,
        });
    }
    if !checkout.is_dir() {
        return Err(format!("{} is not there", checkout.display()));
    }
    git(checkout, &["rev-parse", "--git-dir"])
        .map_err(|e| format!("{} is not a git checkout: {e}", checkout.display()))?;

    let remote = git(checkout, &["remote"])?.lines().any(|r| r == "origin");
    if remote {
        git(checkout, &["fetch", "--quiet", "origin"])
            .map_err(|e| format!("could not fetch from origin: {e}"))?;
    }
    let has =
        |reference: &str| git(checkout, &["rev-parse", "--verify", "--quiet", reference]).is_ok();
    let local = format!("refs/heads/{branch}");
    let pushed = format!("refs/remotes/origin/{branch}");
    let dir_arg = dir.to_string_lossy();
    let reason = format!("agent-host agent {id}");
    let mut add = vec!["worktree", "add", "--lock", "--reason", &reason];
    let base;
    let existed = has(&local);
    if existed {
        add.extend([&*dir_arg, branch.as_str()]);
    } else if remote && has(&pushed) {
        base = format!("origin/{branch}");
        add.extend(["--track", "-b", &branch, &*dir_arg, &base]);
    } else {
        base = if remote && has("refs/remotes/origin/HEAD") {
            "origin/HEAD".to_string()
        } else {
            "HEAD".to_string()
        };
        add.extend(["--no-track", "-b", &branch, &*dir_arg, &base]);
    }
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    // git refuses a branch that another worktree has checked out, which is
    // what keeps two agents placed on one branch out of each other's tree.
    git(checkout, &add)?;
    if existed && remote && has(&pushed) {
        git(
            dir,
            &["merge", "--ff-only", "--quiet", &format!("origin/{branch}")],
        )
        .map_err(|e| format!("{branch} here and on origin have each gone their own way: {e}"))?;
    }
    link_shared(checkout, dir)?;
    Ok(Workspace {
        dir: dir.into(),
        branch,
    })
}

/// Links each shared folder the checkout has. One it does not have is left
/// absent, never linked: a link to nothing reads as missing to whatever
/// follows it and as present to whatever does not, so an install would be
/// skipped by one tool and fail in another.
fn link_shared(checkout: &Path, dir: &Path) -> Result<(), String> {
    for name in SHARED {
        let source = checkout.join(name);
        let target = dir.join(name);
        if !fs::metadata(&source).is_ok_and(|m| m.is_dir()) || target.symlink_metadata().is_ok() {
            continue;
        }
        let source = source
            .canonicalize()
            .map_err(|e| format!("{}: {e}", source.display()))?;
        symlink(&source, &target).map_err(|e| format!("{}: {e}", target.display()))?;
        exclude(dir, name)?;
    }
    Ok(())
}

/// `node_modules/` in a .gitignore matches a folder and not a link to one, so
/// without this an agent's `git add -A` would commit the link. Added to the
/// checkout's own exclude file, which every worktree of it shares, and only
/// added to.
fn exclude(dir: &Path, name: &str) -> Result<(), String> {
    let common = git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let path = Path::new(&common).join("info/exclude");
    let line = format!("/{name}");
    let text = fs::read_to_string(&path).unwrap_or_default();
    if text.lines().any(|l| l == line) {
        return Ok(());
    }
    let mut text = text;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&line);
    text.push('\n');
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Runs git in `dir` with no terminal to ask for credentials on: a fetch that
/// wanted a password would otherwise wait for one for ever, holding the beat.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("git: {e}"))?;
    let (mut stdout, mut stderr) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let out = thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let err = thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + GIT_TIMEOUT;
    let status = loop {
        match child.try_wait().map_err(|e| format!("git: {e}"))? {
            Some(status) => break status,
            None if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "git {}: gave up after {}s",
                    args[0],
                    GIT_TIMEOUT.as_secs()
                ));
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    };
    let (out, err) = (
        out.join().unwrap_or_default(),
        err.join().unwrap_or_default(),
    );
    if status.success() {
        Ok(out.trim().to_string())
    } else {
        Err(format!("git {}: {}", args[0], err.trim()))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A checkout with an origin to fetch from, as a host's would be.
    pub struct Repo {
        pub root: PathBuf,
        pub checkout: PathBuf,
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    pub fn sh(dir: &Path, script: &str) {
        let out = Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{script}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    pub fn repo(name: &str) -> Repo {
        let root =
            std::env::temp_dir().join(format!("agent-host-ws-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        sh(
            &root,
            "git init -q --bare -b main origin.git && git clone -q origin.git checkout 2>/dev/null \
             && cd checkout && echo hello > README && git add README && git commit -qm first \
             && git push -q origin main && git remote set-head origin main",
        );
        Repo {
            checkout: root.join("checkout"),
            root,
        }
    }

    fn out(dir: &Path, args: &[&str]) -> String {
        git(dir, args).unwrap()
    }

    #[test]
    fn an_agent_gets_a_worktree_on_its_own_branch() {
        let repo = repo("own");
        let dir = repo.root.join("agents/a1/checkout");
        let ws = prepare("a1", &repo.checkout, &dir, None).unwrap();
        assert_eq!(ws.branch, "agent/a1");
        assert_eq!(out(&dir, &["branch", "--show-current"]), "agent/a1");
        assert!(dir.join("README").is_file(), "the code is in it");
        assert_eq!(
            out(&repo.checkout, &["branch", "--show-current"]),
            "main",
            "the person's checkout is left as it was"
        );
        let list = out(&repo.checkout, &["worktree", "list", "--porcelain"]);
        assert!(list.contains("locked agent-host agent a1"), "{list}");
    }

    #[test]
    fn two_agents_never_share_a_tree() {
        let repo = repo("two");
        let one = prepare("a1", &repo.checkout, &repo.root.join("a1/c"), None).unwrap();
        let two = prepare("a2", &repo.checkout, &repo.root.join("a2/c"), None).unwrap();
        assert_ne!(one.dir, two.dir);
        assert_ne!(one.branch, two.branch);
        fs::write(one.dir.join("scratch"), "mine").unwrap();
        assert!(!two.dir.join("scratch").exists());

        // Placed on one branch, the second is refused rather than let in.
        prepare(
            "a3",
            &repo.checkout,
            &repo.root.join("a3/c"),
            Some("feat/x"),
        )
        .unwrap();
        let err = prepare(
            "a4",
            &repo.checkout,
            &repo.root.join("a4/c"),
            Some("feat/x"),
        )
        .unwrap_err();
        assert!(err.contains("feat/x"), "{err}");
        assert!(!repo.root.join("a4/c").exists());
    }

    #[test]
    fn a_named_branch_carries_on_from_what_was_pushed() {
        let repo = repo("pushed");
        sh(
            &repo.checkout,
            "git checkout -qb feat/y && echo more > WORK && git add WORK && git commit -qm work \
             && git push -q origin feat/y && git checkout -q main && git branch -qD feat/y",
        );
        let dir = repo.root.join("a1/c");
        prepare("a1", &repo.checkout, &dir, Some("feat/y")).unwrap();
        assert!(dir.join("WORK").is_file());
        assert_eq!(
            out(&dir, &["rev-parse", "--abbrev-ref", "@{upstream}"]),
            "origin/feat/y"
        );
    }

    #[test]
    fn a_local_branch_behind_origin_is_brought_up_to_it() {
        let repo = repo("behind");
        sh(
            &repo.checkout,
            "git branch feat/z && git checkout -q feat/z && echo more > WORK && git add WORK \
             && git commit -qm work && git push -q origin feat/z && git checkout -q main \
             && git branch -qf feat/z main",
        );
        let dir = repo.root.join("a1/c");
        prepare("a1", &repo.checkout, &dir, Some("feat/z")).unwrap();
        assert!(dir.join("WORK").is_file());
    }

    #[test]
    fn a_dependency_folder_is_linked_only_where_there_is_one() {
        let repo = repo("deps");
        let bare = prepare("a1", &repo.checkout, &repo.root.join("a1/c"), None).unwrap();
        assert!(
            bare.dir.join("node_modules").symlink_metadata().is_err(),
            "no link to nothing"
        );

        fs::create_dir(repo.checkout.join("node_modules")).unwrap();
        fs::write(repo.checkout.join("node_modules/dep"), "x").unwrap();
        let with = prepare("a2", &repo.checkout, &repo.root.join("a2/c"), None).unwrap();
        assert!(with.dir.join("node_modules/dep").is_file());
        sh(&with.dir, "git add -A");
        assert_eq!(
            out(&with.dir, &["status", "--porcelain"]),
            "",
            "the link is not part of the work"
        );

        // The checkout's own may itself be a link to nothing.
        fs::remove_dir_all(repo.checkout.join("node_modules")).unwrap();
        symlink("/nonexistent", repo.checkout.join("node_modules")).unwrap();
        let dangling = prepare("a3", &repo.checkout, &repo.root.join("a3/c"), None).unwrap();
        assert!(dangling
            .dir
            .join("node_modules")
            .symlink_metadata()
            .is_err());
    }

    #[test]
    fn a_start_cut_off_halfway_takes_up_the_tree_it_left() {
        let repo = repo("again");
        let dir = repo.root.join("a1/c");
        let first = prepare("a1", &repo.checkout, &dir, None).unwrap();
        fs::write(dir.join("scratch"), "kept").unwrap();
        assert_eq!(prepare("a1", &repo.checkout, &dir, None).unwrap(), first);
        assert!(dir.join("scratch").is_file());
    }

    #[test]
    fn what_cannot_be_prepared_says_why() {
        let repo = repo("cannot");
        let err = prepare(
            "a1",
            &repo.root.join("nothing"),
            &repo.root.join("a1/c"),
            None,
        )
        .unwrap_err();
        assert!(err.contains("is not there"), "{err}");

        fs::create_dir(repo.root.join("plain")).unwrap();
        let err = prepare(
            "a1",
            &repo.root.join("plain"),
            &repo.root.join("a1/c"),
            None,
        )
        .unwrap_err();
        assert!(err.contains("is not a git checkout"), "{err}");

        sh(
            &repo.checkout,
            "git remote set-url origin /nonexistent/origin.git",
        );
        let err = prepare("a1", &repo.checkout, &repo.root.join("a1/c"), None).unwrap_err();
        assert!(err.contains("could not fetch from origin"), "{err}");

        fs::create_dir_all(repo.root.join("a2/c")).unwrap();
        let err = prepare("a2", &repo.checkout, &repo.root.join("a2/c"), None).unwrap_err();
        assert!(err.contains("not a worktree"), "{err}");
    }
}
