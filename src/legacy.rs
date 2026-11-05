//! Taking over from a release of this host that ran each session in tmux.
//! Its sessions may still be running when this one starts, and each is
//! resumed headless (`Workers::reconcile`), so the tmux one must end first:
//! two Claude Codes on one conversation write over each other. Its variables
//! were given to tmux and kept nowhere else, so they are read back from it
//! first. tmux is run only for this, and its absence means there is nothing
//! to take over. Delete this once no host runs a release from before.

use std::collections::HashMap;
use std::process::{Command, Stdio};

/// The session option an older release tagged each session with its worker's
/// id by.
const TAG: &str = "@worker";

/// Ends every session left on the tmux server `socket`, handing back the
/// variables each was started with, by the id of its worker.
pub fn take_over(socket: &str) -> HashMap<String, Vec<(String, String)>> {
    let tmux = |args: &[&str]| -> Option<String> {
        let out = Command::new("tmux")
            .arg("-L")
            .arg(socket)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let Some(sessions) = tmux(&[
        "list-sessions",
        "-F",
        &format!("#{{session_id}} #{{{TAG}}}"),
    ]) else {
        return HashMap::new();
    };
    let carried = sessions
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(_, worker)| !worker.is_empty())
        .map(|(session, worker)| {
            let env = tmux(&["show-environment", "-t", session]).unwrap_or_default();
            (worker.to_string(), variables(&env))
        })
        .collect();
    let _ = tmux(&["kill-server"]);
    carried
}

/// `show-environment`'s lines as variables; a `-NAME` line is one removed.
fn variables(shown: &str) -> Vec<(String, String)> {
    shown
        .lines()
        .filter(|line| !line.starts_with('-'))
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sessions_variables_are_read_as_tmux_shows_them() {
        let shown = "CLAUDE_CONFIG_DIR=/w/ana/home/.claude\n-TERM\nWS_URL=https://ws.example/a=b\n";
        assert_eq!(
            variables(shown),
            [
                ("CLAUDE_CONFIG_DIR".into(), "/w/ana/home/.claude".into()),
                ("WS_URL".into(), "https://ws.example/a=b".into()),
            ]
        );
    }

    #[test]
    fn with_no_tmux_server_there_is_nothing_to_take_over() {
        assert!(take_over(&format!("worker-host-test-{}-none", std::process::id())).is_empty());
    }
}
