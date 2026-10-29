//! What a start carries for the worker beyond its first message: a context,
//! files written into its own Claude Code folder before Claude Code starts,
//! and the environment the context's scripts need. The host knows nothing of
//! where a context comes from; it only writes it.
//!
//! A context writes only `skills/...`, as files, and `CLAUDE.md`, which goes
//! after the host's own rules rather than in place of them. What it wrote is
//! listed in `.context.json`, so the next context first removes it, and a
//! start that carries none keeps the last.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// The file a context writes after the host's rules, not as a file.
pub const MEMORY: &str = "CLAUDE.md";
/// Where a context's files go, under the worker's own Claude Code folder.
const SKILLS: &str = "skills";
/// What the last context wrote, so the next can take it away.
const MANIFEST: &str = ".context.json";
/// Set by the host for every session, so a start may not set them.
const RESERVED: [&str; 2] = ["CLAUDE_CONFIG_DIR", "CLAUDE_SECURESTORAGE_CONFIG_DIR"];

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct File {
    pub path: String,
    pub content: String,
    #[serde(default)]
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Context {
    pub key: String,
    pub version: u64,
    pub files: Vec<File>,
}

/// What `.context.json` holds: the context written last, its paths, and its
/// `CLAUDE.md`, which is written into the rules each start.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Written {
    pub key: String,
    pub version: u64,
    pub paths: Vec<String>,
    pub memory: Option<String>,
}

/// Whether a context may write `path`: `CLAUDE.md`, or a plain relative path
/// under `skills/`.
fn allowed(path: &str) -> bool {
    if path == MEMORY {
        return true;
    }
    // Read as written: `Path` would drop a `.` segment rather than refuse it.
    let parts: Vec<&str> = path.split('/').collect();
    parts.len() > 1
        && parts[0] == SKILLS
        && parts
            .iter()
            .all(|p| !p.is_empty() && *p != "." && *p != "..")
}

/// The context written last into `state`, if any.
pub fn written(state: &Path) -> Option<Written> {
    serde_json::from_str(&fs::read_to_string(state.join(MANIFEST)).ok()?).ok()
}

/// Writes `context` into `state`, a worker's Claude Code folder, in place of
/// the last one: every path is checked before anything is touched, then the
/// last context's files are removed and this one's written.
pub fn write(state: &Path, context: &Context) -> Result<Written, String> {
    if let Some(bad) = context.files.iter().find(|f| !allowed(&f.path)) {
        return Err(format!(
            "the context may write only {MEMORY} and files under {SKILLS}/, not {:?}",
            bad.path
        ));
    }
    let at = |path: &Path, e: std::io::Error| format!("{}: {e}", path.display());
    if let Some(last) = written(state) {
        for path in last.paths {
            let file = state.join(&path);
            match fs::remove_file(&file) {
                Ok(()) => prune(state, &file),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(at(&file, e)),
            }
        }
    }
    let mut memory = None;
    let mut paths = Vec::new();
    for f in &context.files {
        if f.path == MEMORY {
            memory = Some(f.content.clone());
            continue;
        }
        let file = state.join(&f.path);
        if let Some(dir) = file.parent() {
            fs::create_dir_all(dir).map_err(|e| at(dir, e))?;
        }
        fs::write(&file, &f.content).map_err(|e| at(&file, e))?;
        executable(&file, f.executable).map_err(|e| at(&file, e))?;
        paths.push(f.path.clone());
    }
    let done = Written {
        key: context.key.clone(),
        version: context.version,
        paths,
        memory,
    };
    let manifest = state.join(MANIFEST);
    let text = serde_json::to_string_pretty(&done).map_err(|e| e.to_string())?;
    fs::write(&manifest, text + "\n").map_err(|e| at(&manifest, e))?;
    Ok(done)
}

/// Removes the folders a removed file leaves empty, up to `state`.
fn prune(state: &Path, file: &Path) {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if d == state || fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

#[cfg(unix)]
fn executable(file: &Path, yes: bool) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(
        file,
        fs::Permissions::from_mode(if yes { 0o755 } else { 0o644 }),
    )
}

#[cfg(not(unix))]
fn executable(_: &Path, _: bool) -> std::io::Result<()> {
    Ok(())
}

/// The host's rules, then the context's `CLAUDE.md`, if it has one.
pub fn rules(host: &str, written: Option<&Written>) -> String {
    match written.and_then(|w| w.memory.as_deref()) {
        Some(memory) if !memory.trim().is_empty() => format!("{host}\n{memory}"),
        _ => host.to_string(),
    }
}

/// The variables a start sets in the session, but for those the host sets.
pub fn env(given: Option<&BTreeMap<String, String>>) -> Vec<(String, String)> {
    given
        .into_iter()
        .flatten()
        .filter(|(k, _)| !RESERVED.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("worker-host-context-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn file(path: &str, content: &str, executable: bool) -> File {
        File {
            path: path.into(),
            content: content.into(),
            executable,
        }
    }

    fn context(version: u64, files: Vec<File>) -> Context {
        Context {
            key: "global".into(),
            version,
            files,
        }
    }

    #[test]
    fn writes_skills_as_files_scripts_executable_and_keeps_claude_md_for_the_rules() {
        let state = scratch("write");
        let done = write(
            &state,
            &context(
                3,
                vec![
                    file("CLAUDE.md", "Use the tracker.", false),
                    file("skills/wt/SKILL.md", "How.", false),
                    file("skills/wt/bin/wt", "#!/bin/sh", true),
                ],
            ),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(state.join("skills/wt/SKILL.md")).unwrap(),
            "How."
        );
        assert!(!state.join("CLAUDE.md").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &str| fs::metadata(state.join(p)).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                (mode("skills/wt/bin/wt"), mode("skills/wt/SKILL.md")),
                (0o755, 0o644)
            );
        }
        assert_eq!(written(&state).as_ref(), Some(&done));
        assert_eq!(
            rules("Host rules.\n", Some(&done)),
            "Host rules.\n\nUse the tracker."
        );
        assert_eq!(rules("Host rules.\n", None), "Host rules.\n");
    }

    #[test]
    fn a_new_context_takes_away_what_the_last_wrote_and_nothing_else() {
        let state = scratch("replace");
        fs::create_dir_all(state.join("skills/mine")).unwrap();
        fs::write(state.join("skills/mine/SKILL.md"), "the worker's own").unwrap();
        write(
            &state,
            &context(
                1,
                vec![
                    file("skills/old/SKILL.md", "old", false),
                    file("skills/kept/SKILL.md", "v1", false),
                ],
            ),
        )
        .unwrap();
        write(
            &state,
            &context(2, vec![file("skills/kept/SKILL.md", "v2", false)]),
        )
        .unwrap();
        assert!(!state.join("skills/old").exists());
        assert_eq!(
            fs::read_to_string(state.join("skills/kept/SKILL.md")).unwrap(),
            "v2"
        );
        assert_eq!(
            fs::read_to_string(state.join("skills/mine/SKILL.md")).unwrap(),
            "the worker's own"
        );
        assert_eq!(written(&state).unwrap().version, 2);
    }

    #[test]
    fn refuses_anything_outside_skills_before_touching_a_file() {
        let state = scratch("refuse");
        write(
            &state,
            &context(1, vec![file("skills/a/SKILL.md", "a", false)]),
        )
        .unwrap();
        for bad in [
            "settings.json",
            "skills",
            "skills/../settings.json",
            "/etc/passwd",
            "skills/./a",
            ".context.json",
            "",
        ] {
            let err = write(
                &state,
                &context(
                    2,
                    vec![file("skills/b/SKILL.md", "b", false), file(bad, "x", false)],
                ),
            )
            .unwrap_err();
            assert!(err.contains("may write only"), "{bad}: {err}");
        }
        assert!(state.join("skills/a/SKILL.md").exists());
        assert!(!state.join("skills/b").exists());
    }

    #[test]
    fn a_start_sets_its_variables_but_never_the_hosts_own() {
        let given: BTreeMap<String, String> = [
            ("DS_URL", "https://ds"),
            ("CLAUDE_CONFIG_DIR", "/elsewhere"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .into();
        assert_eq!(
            env(Some(&given)),
            vec![("DS_URL".into(), "https://ds".into())]
        );
        assert_eq!(env(None), vec![]);
    }
}
