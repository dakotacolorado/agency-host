//! What a start carries for the worker beyond its first message: a context,
//! files written into its own Claude Code folder before Claude Code starts,
//! and the environment the context's scripts need. The host knows nothing of
//! where a context comes from; it only writes it.
//!
//! A context writes only `skills/...`, as files, and `CLAUDE.md`, which goes
//! after the host's own rules rather than in place of them. A start may carry
//! documents beside it, written after it the same way, each `CLAUDE.md` after
//! the one before. What they wrote is listed in `.context.json`, the context's
//! at the top as it always was and the documents' under `documents`, so the
//! next start first removes it, and a start that carries no context keeps the
//! last.

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
/// `CLAUDE.md`, which is written into the rules each start; then the same of
/// each document written beside it. The context's fields come first, so a
/// script that reads the first `key` and `version` reads the context's.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Written {
    pub key: String,
    pub version: u64,
    pub paths: Vec<String>,
    pub memory: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub documents: Vec<Written>,
}

impl Written {
    fn all(&self) -> impl Iterator<Item = &Written> {
        std::iter::once(self).chain(&self.documents)
    }
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

/// Writes `context`, then `documents`, into `state`, a worker's Claude Code
/// folder, in place of the last: every path is checked before anything is
/// touched, then the last start's files are removed and these written.
pub fn write(state: &Path, context: &Context, documents: &[Context]) -> Result<Written, String> {
    let mut seen = std::collections::HashSet::new();
    for c in std::iter::once(context).chain(documents) {
        if let Some(bad) = c.files.iter().find(|f| !allowed(&f.path)) {
            return Err(format!(
                "{} may write only {MEMORY} and files under {SKILLS}/, not {:?}",
                c.key, bad.path
            ));
        }
        if let Some(twice) = c
            .files
            .iter()
            .find(|f| f.path != MEMORY && !seen.insert(f.path.as_str()))
        {
            return Err(format!(
                "{} writes {}, as a document before it does",
                c.key, twice.path
            ));
        }
    }
    let at = |path: &Path, e: std::io::Error| format!("{}: {e}", path.display());
    if let Some(last) = written(state) {
        for path in last.all().flat_map(|w| &w.paths) {
            let file = state.join(path);
            match fs::remove_file(&file) {
                Ok(()) => prune(state, &file),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(at(&file, e)),
            }
        }
    }
    let mut done = put(state, context)?;
    for d in documents {
        done.documents.push(put(state, d)?);
    }
    let manifest = state.join(MANIFEST);
    let text = serde_json::to_string_pretty(&done).map_err(|e| e.to_string())?;
    fs::write(&manifest, text + "\n").map_err(|e| at(&manifest, e))?;
    Ok(done)
}

/// Writes one context's files, its paths checked already.
fn put(state: &Path, context: &Context) -> Result<Written, String> {
    let at = |path: &Path, e: std::io::Error| format!("{}: {e}", path.display());
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
    Ok(Written {
        key: context.key.clone(),
        version: context.version,
        paths,
        memory,
        documents: Vec::new(),
    })
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

/// The host's rules, then the context's `CLAUDE.md` and each document's,
/// those that have one.
pub fn rules(host: &str, written: Option<&Written>) -> String {
    written
        .into_iter()
        .flat_map(Written::all)
        .filter_map(|w| w.memory.as_deref())
        .filter(|memory| !memory.trim().is_empty())
        .fold(host.to_string(), |rules, memory| {
            format!("{rules}\n{memory}")
        })
}

/// Writes `secrets` to `file` as shell assignments the session reads as it
/// starts, readable by the host's user alone; with none, removes the last.
/// Each time a session starts it reads them again, so a session resumed
/// after the host restarts has what its start gave it.
pub fn credentials(file: &Path, secrets: Option<&BTreeMap<String, String>>) -> Result<(), String> {
    let at = |e: std::io::Error| format!("{}: {e}", file.display());
    let secrets: Vec<_> = env(secrets);
    if secrets.is_empty() {
        return match fs::remove_file(file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(at(e)),
            _ => Ok(()),
        };
    }
    if let Some((bad, _)) = secrets.iter().find(|(k, _)| !variable(k)) {
        return Err(format!("{bad:?} is not a variable a session can be given"));
    }
    let text: String = secrets
        .iter()
        .map(|(k, v)| format!("{k}='{}'\n", v.replace('\'', r"'\''")))
        .collect();
    private(file, text.as_bytes()).map_err(at)
}

/// What `credentials` wrote to `file`, read back.
#[cfg(test)]
pub fn written_credentials(file: &Path) -> BTreeMap<String, String> {
    let text = fs::read_to_string(file).unwrap_or_default();
    text.lines()
        .filter_map(|line| {
            let (k, v) = line.split_once('=')?;
            let v = v.strip_prefix('\'')?.strip_suffix('\'')?;
            Some((k.to_string(), v.replace(r"'\''", "'")))
        })
        .collect()
}

fn variable(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Written to a file made, or made again, readable by its owner alone before
/// anything is in it.
#[cfg(unix)]
fn private(file: &Path, content: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if file.exists() {
        fs::set_permissions(file, fs::Permissions::from_mode(0o600))?;
    }
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(file)?
        .write_all(content)
}

#[cfg(not(unix))]
fn private(file: &Path, content: &[u8]) -> std::io::Result<()> {
    fs::write(file, content)
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
            &[],
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
            &[],
        )
        .unwrap();
        write(
            &state,
            &context(2, vec![file("skills/kept/SKILL.md", "v2", false)]),
            &[],
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
            &[],
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
                &[],
            )
            .unwrap_err();
            assert!(err.contains("may write only"), "{bad}: {err}");
        }
        assert!(state.join("skills/a/SKILL.md").exists());
        assert!(!state.join("skills/b").exists());
    }

    #[test]
    fn documents_go_after_the_context_and_are_taken_away_with_it() {
        let state = scratch("documents");
        let approval = Context {
            key: "approval".into(),
            version: 2,
            files: vec![
                file("CLAUDE.md", "Approve.", false),
                file("skills/approve/SKILL.md", "How.", false),
            ],
        };
        let done = write(
            &state,
            &context(1, vec![file("CLAUDE.md", "Shared.", false)]),
            std::slice::from_ref(&approval),
        )
        .unwrap();
        assert_eq!(rules("Host.\n", Some(&done)), "Host.\n\nShared.\nApprove.");
        assert_eq!(written(&state).as_ref(), Some(&done));
        assert!(state.join("skills/approve/SKILL.md").exists());
        let alone = write(&state, &context(2, vec![]), &[]).unwrap();
        assert!(!state.join("skills/approve").exists());
        assert_eq!(rules("Host.\n", Some(&alone)), "Host.\n");
    }

    #[test]
    fn refuses_two_documents_writing_one_file_before_touching_a_file() {
        let state = scratch("twice");
        let mine = |key: &str| Context {
            key: key.into(),
            version: 1,
            files: vec![file("skills/x/SKILL.md", key, false)],
        };
        let err = write(&state, &mine("global"), &[mine("approval")]).unwrap_err();
        assert_eq!(
            err,
            "approval writes skills/x/SKILL.md, as a document before it does"
        );
        assert!(!state.join("skills").exists());
    }

    #[test]
    fn tokens_go_to_a_file_only_its_owner_reads_quoted_as_given_and_away_with_none() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("credentials");
        let file = dir.join("credentials.env");
        fs::write(&file, "OLD=1").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        let given: BTreeMap<String, String> = [
            ("WT_TOKEN", "it's $x `y`"),
            ("CLAUDE_CONFIG_DIR", "/elsewhere"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .into();
        credentials(&file, Some(&given)).unwrap();
        assert_eq!(
            written_credentials(&file),
            [("WT_TOKEN".to_string(), "it's $x `y`".to_string())].into(),
            "read back as given, but for the host's own"
        );
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "WT_TOKEN='it'\\''s $x `y`'\n"
        );
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let bad: BTreeMap<String, String> = [("A-B".to_string(), "x".to_string())].into();
        assert!(credentials(&file, Some(&bad)).is_err());
        credentials(&file, None).unwrap();
        assert!(!file.exists());
        credentials(&file, None).unwrap();
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
