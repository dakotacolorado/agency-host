//! Agents run in tmux because an interactive agent needs a terminal -- given a
//! pipe instead, Claude Code runs once and exits -- and because a session
//! outlives the host: a host that restarts finds its agents still running.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// One tmux server on its own socket, so the host's sessions are kept apart
/// from any a person runs on the same machine. A person attaches with
/// `tmux -L <socket> attach -t <agent>`.
pub struct Tmux {
    /// None is the server a person gets from a bare `tmux`.
    socket: Option<String>,
}

/// The session option that names the agent a session runs, so a session is
/// found by the agent's id exactly, whatever tmux made of its name.
const TAG: &str = "@agent";

impl Tmux {
    pub fn new(socket: impl Into<String>) -> Self {
        Tmux {
            socket: Some(socket.into()),
        }
    }

    /// The server a person's own sessions run on, which the host only lists.
    pub fn persons() -> Self {
        Tmux { socket: None }
    }

    fn run(&self, args: &[&str], input: Option<&str>) -> Result<String, String> {
        let mut command = Command::new("tmux");
        if let Some(socket) = &self.socket {
            command.arg("-L").arg(socket);
        }
        let mut child = command
            .args(args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("tmux: {e}"))?;
        if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
            stdin
                .write_all(input.as_bytes())
                .map_err(|e| format!("tmux {}: {e}", args[0]))?;
        }
        let out = child.wait_with_output().map_err(|e| format!("tmux: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(format!(
                "tmux {}: {}",
                args[0],
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    /// Runs `argv` detached in `dir`, as the session for `tag`. A second
    /// session for the same tag is refused by tmux as a duplicate name.
    pub fn start(
        &self,
        tag: &str,
        dir: &Path,
        argv: &[String],
        env: &[(String, String)],
    ) -> Result<(), String> {
        let name: String = tag
            .chars()
            .map(|c| match c {
                'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => c,
                _ => '_',
            })
            .collect();
        let dir = dir.to_string_lossy();
        let env: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let mut args = vec!["new-session", "-d", "-P", "-F", "#{session_id}"];
        args.extend(["-s", &name, "-c", &dir]);
        for pair in &env {
            args.extend(["-e", pair]);
        }
        // More than one argument is exec'd as it stands, never handed to a
        // shell, so nothing in the instructions is read as shell syntax.
        args.extend(argv.iter().map(String::as_str));
        let id = self.run(&args, None)?;
        self.run(&["set-option", "-t", id.trim(), TAG, tag], None)?;
        Ok(())
    }

    /// The tags of every session running, mapped to tmux's id for each.
    pub fn sessions(&self) -> Vec<(String, String)> {
        // With no session there is no server, and asking is an error.
        let Ok(out) = self.run(
            &[
                "list-sessions",
                "-F",
                &format!("#{{session_id}} #{{{TAG}}}"),
            ],
            None,
        ) else {
            return vec![];
        };
        out.lines()
            .filter_map(|line| line.split_once(' '))
            .filter(|(_, tag)| !tag.is_empty())
            .map(|(id, tag)| (tag.to_string(), id.to_string()))
            .collect()
    }

    /// The name of every session on this server, or None when tmux could not
    /// be asked. A server that is not running has no sessions, which is an
    /// answer, not a failure to read one.
    pub fn names(&self) -> Option<Vec<String>> {
        match self.run(&["list-sessions", "-F", "#{session_name}"], None) {
            Ok(out) => Some(out.lines().map(str::to_string).collect()),
            Err(e)
                if e.contains("no server running")
                    || (e.contains("error connecting to") && e.contains("No such file")) =>
            {
                Some(vec![])
            }
            Err(_) => None,
        }
    }

    /// Types `text` into the session as one paste, then presses Enter. Pasted
    /// rather than typed so that a newline inside the text does not send what
    /// comes before it on its own.
    pub fn type_in(&self, session: &str, text: &str) -> Result<(), String> {
        let buffer = format!("agent-host{session}");
        self.run(&["load-buffer", "-b", &buffer, "-"], Some(text))?;
        self.run(
            &["paste-buffer", "-p", "-d", "-b", &buffer, "-t", session],
            None,
        )?;
        self.press(session, "Enter")
    }

    /// Ends the session and what runs in it, which tmux hangs up on.
    pub fn kill_session(&self, session: &str) -> Result<(), String> {
        self.run(&["kill-session", "-t", session], None)?;
        Ok(())
    }

    /// Presses one key in the session, named as `send-keys` names it.
    pub fn press(&self, session: &str, key: &str) -> Result<(), String> {
        self.run(&["send-keys", "-t", session, key], None)?;
        Ok(())
    }

    #[cfg(test)]
    pub fn kill_server(&self) {
        let _ = self.run(&["kill-server"], None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_not_running_has_no_sessions() {
        let tmux = Tmux::new(format!("agent-host-test-{}-names-none", std::process::id()));
        assert_eq!(tmux.names(), Some(vec![]));
    }

    #[test]
    fn every_session_is_named() {
        let tmux = Tmux::new(format!("agent-host-test-{}-names", std::process::id()));
        let argv = ["sleep".to_string(), "30".to_string()];
        tmux.start("a.1", Path::new("/"), &argv, &[]).unwrap();
        let names = tmux.names();
        tmux.kill_server();
        assert_eq!(names, Some(vec!["a_1".to_string()]));
    }
}
