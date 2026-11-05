//! A worker's folder on this host, `<workers_dir>/<name>/`, and everything the
//! host keeps about the worker there:
//!
//! ```text
//! worker.json       who it is: id and name, as created
//! sessions.jsonl    each session the host started for it, and each it stopped
//! prompt.md         the opening message of its latest session
//! home/.claude/     its own Claude Code: settings, rules, history, transcripts
//! logs/<id>.jsonl   everything each session said on its stdout, as it came
//! logs/<id>.log     everything each session wrote on its stderr
//! ```
//!
//! The host remembers nothing between runs that is not here, so a machine
//! that dies at any point leaves a folder the next run can read.

use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Where retired workers' folders are moved, out of the way of a new worker
/// given the same name. Hidden, so it is never taken for a worker named so.
pub const RETIRED: &str = ".retired";

/// Who a worker is, as its `create` said.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub name: String,
}

/// One session the host started for a worker. `claude` is the Claude Code
/// session it carries, and `from` how long Claude Code's own transcript of
/// it was when it began: a session that carries on the last conversation
/// writes on in the same transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub session_id: String,
    pub claude: String,
    pub from: u64,
    /// Whether it runs headless, its events kept in `Home::events`. What a
    /// session an older release began said is in Claude Code's transcript,
    /// up to where a headless one carried it on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
    /// Whether it was ended by a `stop`, rather than by whatever else can end
    /// a session: a crash, the machine going down.
    #[serde(skip)]
    pub stopped: bool,
    /// Whether the host saw it end without a stop. A session that was live
    /// when the host went away is neither, and is resumed when it is back.
    #[serde(skip)]
    pub ended: bool,
}

/// A line of `sessions.jsonl`: a session begun, one stopped, one that ended
/// by itself, or one that never began after all, since it would not run. A
/// session an older release began is begun again, headless, under its own id.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Event {
    Begun(Session),
    #[serde(rename_all = "camelCase")]
    Stopped {
        session_id: String,
        stopped: bool,
    },
    #[serde(rename_all = "camelCase")]
    Ended {
        session_id: String,
        ended: bool,
    },
    #[serde(rename_all = "camelCase")]
    Withdrawn {
        session_id: String,
        withdrawn: bool,
    },
}

/// A worker name as the service allows it, which is also all that keeps a
/// name from reaching outside the workers folder: `[a-z0-9][a-z0-9-]{0,39}`.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.len() <= 40
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[derive(Debug, Clone, PartialEq)]
pub struct Home {
    pub dir: PathBuf,
}

impl Home {
    pub fn new(dir: PathBuf) -> Self {
        Home { dir }
    }

    /// The worker's own HOME as the layout names it. HOME itself stays the
    /// host user's, for the reason in claude.rs; this holds the worker's own
    /// Claude Code state.
    pub fn home(&self) -> PathBuf {
        self.dir.join("home")
    }

    pub fn claude(&self) -> PathBuf {
        self.home().join(".claude")
    }

    /// The session's stdout, a JSON line per event, kept as it came.
    pub fn events(&self, session: &str) -> PathBuf {
        self.dir.join("logs").join(format!("{session}.jsonl"))
    }

    /// The session's stderr.
    pub fn errors(&self, session: &str) -> PathBuf {
        self.dir.join("logs").join(format!("{session}.log"))
    }

    pub fn prompt(&self) -> PathBuf {
        self.dir.join("prompt.md")
    }

    /// The session's tokens, read into its environment as it starts.
    pub fn credentials(&self) -> PathBuf {
        self.dir.join("credentials.env")
    }

    fn record_file(&self) -> PathBuf {
        self.dir.join("worker.json")
    }

    fn sessions_file(&self) -> PathBuf {
        self.dir.join("sessions.jsonl")
    }

    pub fn record(&self) -> Option<Record> {
        serde_json::from_str(&fs::read_to_string(self.record_file()).ok()?).ok()
    }

    /// Makes the folder for `record`, or takes up the one a create that was
    /// cut off, or sent again, left. One that belongs to another worker is
    /// refused: the service keeps names unique among workers not retired, and
    /// a retired worker's folder is moved aside, so this is a worker the
    /// service no longer knows about, and its folder is not this host's to
    /// take.
    pub fn make(&self, record: &Record) -> Result<(), String> {
        let at = |path: &Path, e: io::Error| format!("{}: {e}", path.display());
        if let Some(held) = self.record() {
            if held.id != record.id {
                return Err(format!(
                    "{} already holds worker {} by that name",
                    self.dir.display(),
                    held.id
                ));
            }
        }
        for dir in [self.home(), self.dir.join("logs")] {
            fs::create_dir_all(&dir).map_err(|e| at(&dir, e))?;
        }
        let file = self.record_file();
        let text = serde_json::to_string_pretty(record).map_err(|e| e.to_string())?;
        fs::write(&file, text + "\n").map_err(|e| at(&file, e))
    }

    /// Every session the host has started for the worker, oldest first. A
    /// line cut off by the machine going down mid-write is passed over.
    pub fn sessions(&self) -> Vec<Session> {
        let text = fs::read_to_string(self.sessions_file()).unwrap_or_default();
        let mut sessions: Vec<Session> = Vec::new();
        for line in text.lines() {
            match serde_json::from_str(line) {
                Ok(Event::Begun(session)) => sessions.push(session),
                Ok(Event::Stopped { session_id, .. }) => sessions
                    .iter_mut()
                    .filter(|s| s.session_id == session_id)
                    .for_each(|s| s.stopped = true),
                Ok(Event::Ended { session_id, .. }) => sessions
                    .iter_mut()
                    .filter(|s| s.session_id == session_id)
                    .for_each(|s| s.ended = true),
                Ok(Event::Withdrawn { session_id, .. }) => {
                    sessions.retain(|s| s.session_id != session_id)
                }
                Err(_) => {}
            }
        }
        sessions
    }

    pub fn latest(&self) -> Option<Session> {
        self.sessions().pop()
    }

    pub fn begin(&self, session: &Session) -> Result<(), String> {
        self.append(&Event::Begun(session.clone()))
    }

    pub fn stopped(&self, session_id: &str) -> Result<(), String> {
        self.append(&Event::Stopped {
            session_id: session_id.into(),
            stopped: true,
        })
    }

    pub fn ended(&self, session_id: &str) -> Result<(), String> {
        self.append(&Event::Ended {
            session_id: session_id.into(),
            ended: true,
        })
    }

    /// Takes back a session that would not start, so that the start is tried
    /// again when it is sent again rather than taken for one already done.
    pub fn withdraw(&self, session_id: &str) -> Result<(), String> {
        self.append(&Event::Withdrawn {
            session_id: session_id.into(),
            withdrawn: true,
        })
    }

    fn append(&self, event: &Event) -> Result<(), String> {
        let file = self.sessions_file();
        let mut line = serde_json::to_string(event).map_err(|e| e.to_string())? + "\n";
        // After a line torn by the machine going down, this one starts afresh
        // rather than being read as the end of that one.
        if fs::read(&file).is_ok_and(|t| t.last().is_some_and(|&b| b != b'\n')) {
            line.insert(0, '\n');
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file)
            .and_then(|mut f| f.write_all(line.as_bytes()))
            .map_err(|e| format!("{}: {e}", file.display()))
    }

    /// Moves the worker's folder to `<workers_dir>/.retired/<name>-<id>`,
    /// where it is kept, so a new worker can be given its name.
    pub fn retire(&self, record: &Record) -> Result<(), String> {
        let parent = self.dir.parent().ok_or("a worker folder has no parent")?;
        let aside = parent
            .join(RETIRED)
            .join(format!("{}-{}", record.name, record.id));
        fs::create_dir_all(parent.join(RETIRED))
            .map_err(|e| format!("{}: {e}", parent.join(RETIRED).display()))?;
        fs::rename(&self.dir, &aside).map_err(|e| format!("{}: {e}", self.dir.display()))
    }
}

/// Every worker with a folder under `dir`, retired ones aside.
pub fn all(dir: &Path) -> Vec<(Home, Record)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return vec![];
    };
    let mut found: Vec<(Home, Record)> = entries
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| Home::new(e.path()))
        .filter_map(|home| home.record().map(|r| (home, r)))
        .collect();
    found.sort_by(|a, b| a.1.name.cmp(&b.1.name));
    found
}

/// Whether a worker with this id was retired here.
pub fn retired(dir: &Path, id: &str) -> bool {
    let suffix = format!("-{id}");
    fs::read_dir(dir.join(RETIRED)).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|e| e.file_name().to_string_lossy().ends_with(&suffix))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("worker-host-home-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ana() -> Record {
        Record {
            id: "w1".into(),
            name: "ana".into(),
        }
    }

    #[test]
    fn a_name_is_a_slug_and_nothing_that_reaches_outside() {
        for good in ["ana", "a", "0", "front-end-2", &"a".repeat(40)] {
            assert!(valid_name(good), "{good}");
        }
        for bad in [
            "",
            "-ana",
            "Ana",
            "../x",
            "a/b",
            ".retired",
            "a b",
            &"a".repeat(41),
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_worker_folder_has_the_layout_the_protocol_names() {
        let dir = scratch("layout");
        let home = Home::new(dir.join("ana"));
        home.make(&ana()).unwrap();
        for sub in ["home", "logs"] {
            assert!(dir.join("ana").join(sub).is_dir(), "{sub}");
        }
        assert_eq!(home.claude(), dir.join("ana/home/.claude"));
        assert_eq!(home.events("s1"), dir.join("ana/logs/s1.jsonl"));
        assert_eq!(home.errors("s1"), dir.join("ana/logs/s1.log"));
        assert_eq!(home.record(), Some(ana()));
        assert_eq!(all(&dir), [(home.clone(), ana())]);

        home.make(&ana()).unwrap();
        let err = home
            .make(&Record {
                id: "w2".into(),
                ..ana()
            })
            .unwrap_err();
        assert!(err.contains("already holds worker w1"), "{err}");
    }

    #[test]
    fn sessions_are_read_back_as_written_and_a_torn_line_is_passed_over() {
        let dir = scratch("sessions");
        let home = Home::new(dir.join("ana"));
        home.make(&ana()).unwrap();
        assert_eq!(home.latest(), None);
        let one = Session {
            session_id: "s1".into(),
            claude: "c1".into(),
            from: 0,
            stream: false,
            stopped: false,
            ended: false,
        };
        home.begin(&one).unwrap();
        home.stopped("s1").unwrap();
        let two = Session {
            session_id: "s2".into(),
            claude: "c1".into(),
            from: 120,
            stream: true,
            stopped: false,
            ended: false,
        };
        home.begin(&two).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(dir.join("ana/sessions.jsonl"))
            .unwrap()
            .write_all(br#"{"sessionId":"s3","cla"#)
            .unwrap();
        home.stopped("s2").unwrap();
        home.begin(&Session {
            session_id: "s4".into(),
            claude: "c4".into(),
            from: 0,
            stream: true,
            stopped: false,
            ended: false,
        })
        .unwrap();
        home.withdraw("s4").unwrap();
        assert_eq!(
            home.sessions(),
            [
                Session {
                    stopped: true,
                    ..one
                },
                Session {
                    stopped: true,
                    ..two.clone()
                }
            ]
        );
        assert_eq!(home.latest().map(|s| s.session_id), Some("s2".into()));
    }

    #[test]
    fn a_session_an_older_release_began_is_begun_again_and_ends_or_stops_as_one() {
        let dir = scratch("carried");
        let home = Home::new(dir.join("ana"));
        home.make(&ana()).unwrap();
        // As an older release wrote it.
        fs::write(
            dir.join("ana/sessions.jsonl"),
            "{\"sessionId\":\"s1\",\"claude\":\"c1\",\"from\":0}\n",
        )
        .unwrap();
        let older = Session {
            session_id: "s1".into(),
            claude: "c1".into(),
            from: 0,
            stream: false,
            stopped: false,
            ended: false,
        };
        assert_eq!(home.sessions(), std::slice::from_ref(&older));
        let headless = Session {
            from: 300,
            stream: true,
            ..older.clone()
        };
        home.begin(&headless).unwrap();
        home.ended("s1").unwrap();
        let line = fs::read_to_string(dir.join("ana/sessions.jsonl")).unwrap();
        assert!(line.contains(r#""stream":true"#), "{line}");
        let ended: Vec<bool> = home.sessions().iter().map(|s| s.ended).collect();
        assert_eq!(ended, [true, true]);
        home.stopped("s1").unwrap();
        assert!(home.sessions().iter().all(|s| s.stopped));
    }
}
