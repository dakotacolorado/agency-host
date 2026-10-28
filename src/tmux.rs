//! Workers' sessions run in tmux because an interactive Claude Code needs a
//! terminal -- given a pipe instead, it runs once and exits -- and because a
//! session outlives the host: a host that restarts finds its sessions still
//! running.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// One tmux server on its own socket, so the host's sessions are kept apart
/// from any a person runs on the same machine. A person attaches with
/// `tmux -L <socket> attach -t worker-<name>`.
pub struct Tmux {
    socket: String,
}

/// The session option that names the worker a session is for, so a session is
/// found by the worker's id exactly, whatever tmux made of its name.
const TAG: &str = "@worker";

impl Tmux {
    pub fn new(socket: impl Into<String>) -> Self {
        Tmux {
            socket: socket.into(),
        }
    }

    fn run(&self, args: &[&str], input: Option<&str>) -> Result<String, String> {
        let mut child = Command::new("tmux")
            .arg("-L")
            .arg(&self.socket)
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

    /// Runs `argv` detached in `dir`, as the session `name` for `tag`, and
    /// hands back tmux's id for it. A second session of the same name is
    /// refused by tmux as a duplicate.
    pub fn start(
        &self,
        tag: &str,
        name: &str,
        dir: &Path,
        argv: &[String],
        env: &[(String, String)],
    ) -> Result<String, String> {
        let name: String = name
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
        let id = self.run(&args, None)?.trim().to_string();
        self.run(&["set-option", "-t", &id, TAG, tag], None)?;
        Ok(id)
    }

    /// Appends everything the session's pane shows to `log`, from now on.
    /// `-o` opens a pipe only where there is none, so a host that restarts
    /// and asks again does not end the one already writing.
    pub fn log_to(&self, session: &str, log: &Path) -> Result<(), String> {
        let quoted = format!("'{}'", log.to_string_lossy().replace('\'', r"'\''"));
        self.run(
            &[
                "pipe-pane",
                "-o",
                "-t",
                session,
                &format!("cat >> {quoted}"),
            ],
            None,
        )?;
        Ok(())
    }

    /// What the session's pane shows now, as text.
    pub fn screen(&self, session: &str) -> Option<String> {
        self.run(&["capture-pane", "-p", "-t", session], None).ok()
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

    /// Types `text` into the session as one paste, then presses Enter. Pasted
    /// rather than typed so that a newline inside the text does not send what
    /// comes before it on its own.
    pub fn type_in(&self, session: &str, text: &str) -> Result<(), String> {
        let buffer = format!("worker-host{session}");
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
