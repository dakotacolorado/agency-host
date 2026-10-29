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
    /// refused by tmux as a duplicate. With a `log`, everything the pane
    /// shows goes to it from the program's first word.
    pub fn start(
        &self,
        tag: &str,
        name: &str,
        dir: &Path,
        argv: &[String],
        env: &[(String, String)],
        log: Option<&Path>,
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
        // A pipe can only be attached to a pane that exists, and a program
        // started with the pane can speak before it is: so the pane first
        // holds a program that says nothing, and the real one replaces it.
        const QUIET: [&str; 2] = ["sleep", "86400"];
        let first: Vec<&str> = match log {
            Some(_) => QUIET.to_vec(),
            None => argv.iter().map(String::as_str).collect(),
        };
        // More than one argument is exec'd as it stands, never handed to a
        // shell, so nothing in the instructions is read as shell syntax.
        args.extend(first);
        let id = self.run(&args, None)?.trim().to_string();
        self.run(&["set-option", "-t", &id, TAG, tag], None)?;
        if let Some(log) = log {
            if let Err(e) = self.log_to(&id, log) {
                crate::log!("session {name}: its terminal is not logged: {e}");
            }
            let mut respawn = vec!["respawn-pane", "-k", "-t", &id, "-c", &dir];
            respawn.extend(argv.iter().map(String::as_str));
            if let Err(e) = self.run(&respawn, None) {
                let _ = self.kill_session(&id);
                return Err(e);
            }
        }
        Ok(id)
    }

    /// Appends everything the session's pane shows to `log`, from now on.
    /// `pipe-pane -o` is a toggle: on a pane that already has a pipe it closes
    /// it and opens none. A host that restarts asks again of panes still
    /// writing to the last host's pipe, so those are left as they are.
    pub fn log_to(&self, session: &str, log: &Path) -> Result<(), String> {
        let piped = self.run(
            &["display-message", "-p", "-t", session, "#{pane_pipe}"],
            None,
        )?;
        if piped.trim() == "1" {
            return Ok(());
        }
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
        // Claude Code puts an interrupted prompt back in its input; cleared
        // first, it cannot be sent again as the start of this message.
        self.press(session, "C-u")?;
        let buffer = format!("worker-host{session}");
        self.run(&["load-buffer", "-b", &buffer, "-"], Some(text))?;
        self.run(
            &["paste-buffer", "-p", "-d", "-b", &buffer, "-t", session],
            None,
        )?;
        self.press(session, "Enter")
    }

    /// What the session's terminal shows now, as text.
    pub fn screen(&self, session: &str) -> Result<String, String> {
        self.run(&["capture-pane", "-p", "-t", session], None)
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
