use super::*;
use crate::commands;
use crate::protocol::Command;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::time::{Duration, Instant};
use std::{fs, thread};

/// What every stand-in for Claude Code starts with: it takes its arguments as
/// the host gives them -- a new session by id, or the folder's last one
/// carried on -- and writes where Claude Code would, in the worker's own
/// state. `$1` is then the opening message, and `w` appends a transcript line.
const PRELUDE: &str = r#"#!/bin/sh
dir="$CLAUDE_CONFIG_DIR/projects/-fake"; mkdir -p "$dir"
[ "$1" = --dangerously-skip-permissions ] || exit 64
case "$2" in
  --session-id) s=$3; shift 3 ;;
  --continue) s=$(cat "$dir/latest"); shift 2 ;;
  *) exit 64 ;;
esac
printf '%s' "$s" > "$dir/latest"; t="$dir/$s.jsonl"
pwd > "$dir/$s.cwd"
w() { printf '%s\n' "$1" >> "$t"; }
"#;

/// Works one step, then takes what was typed while it worked -- its safe
/// point -- and goes on; after that it stops and answers each line typed.
const FAKE_CLAUDE: &str = r#"
w "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"$(printf '%s\n' "$1" | tail -n 1)\"}}"
w '{"type":"assistant","message":{"id":"m0","content":[{"type":"text","text":"step one"}],"stop_reason":"tool_use"}}'
sleep 1
IFS= read -r line
w "{\"type\":\"attachment\",\"attachment\":{\"type\":\"queued_command\",\"prompt\":\"$line\"}}"
w '{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"which branch?"}],"stop_reason":"end_turn"}}'
n=1
while IFS= read -r line; do
  n=$((n+1))
  w "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"$line\"}}"
  w "{\"type\":\"assistant\",\"message\":{\"id\":\"m$n\",\"content\":[{\"type\":\"text\",\"text\":\"on $line\"}],\"stop_reason\":\"end_turn\"}}"
done
"#;

/// Answers its opening message and each line typed, and ends each turn.
const CHAT_CLAUDE: &str = r#"
w "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"$1\"}}"
w "{\"type\":\"assistant\",\"message\":{\"id\":\"m0\",\"content\":[{\"type\":\"text\",\"text\":\"on $1\"}],\"stop_reason\":\"end_turn\"}}"
echo "said: on $1"
while IFS= read -r line; do
  w "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"$line\"}}"
  w "{\"type\":\"assistant\",\"message\":{\"id\":\"m1\",\"content\":[{\"type\":\"text\",\"text\":\"on $line\"}],\"stop_reason\":\"end_turn\"}}"
done
"#;

/// A turn that runs until it is stopped: it gives up its turn only on
/// Escape, and then answers each line typed.
const BUSY_CLAUDE: &str = r#"
stty -icanon -echo min 1
w '{"type":"assistant","message":{"id":"m0","content":[{"type":"text","text":"working"}],"stop_reason":"tool_use"}}'
[ "$(dd bs=1 count=1 2>/dev/null)" = "$(printf '\033')" ] || exit 65
w '{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}'
stty icanon
n=0
while IFS= read -r line; do
  n=$((n+1))
  w "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"$line\"}}"
  w "{\"type\":\"assistant\",\"message\":{\"id\":\"m$n\",\"content\":[{\"type\":\"text\",\"text\":\"on $line\"}],\"stop_reason\":\"end_turn\"}}"
done
"#;

/// As Claude Code starts: what is typed before it is up is thrown away, as a
/// terminal switched into raw mode discards it.
const DEAF_CLAUDE: &str = r#"
sleep 1
stty -icanon min 0 time 0; cat > /dev/null; stty icanon
w '{"type":"user","message":{"role":"user","content":"wait to be told"}}'
w '{"type":"assistant","message":{"id":"m0","content":[{"type":"text","text":"waiting"}],"stop_reason":"end_turn"}}'
n=0
while IFS= read -r line; do
  n=$((n+1))
  w "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"$line\"}}"
  w "{\"type\":\"assistant\",\"message\":{\"id\":\"m$n\",\"content\":[{\"type\":\"text\",\"text\":\"on $line\"}],\"stop_reason\":\"end_turn\"}}"
done
"#;

/// Never gets far enough to write a transcript.
const STUCK_CLAUDE: &str = "sleep 60\n";

/// Answers once and then never reads what is typed to it.
const AWAY_CLAUDE: &str = r#"
w '{"type":"assistant","message":{"id":"m0","content":[{"type":"text","text":"busy"}],"stop_reason":"tool_use"}}'
sleep 60
"#;

/// Records Claude Code's part of its environment, then waits.
const ENV_CLAUDE: &str = r#"
printf '%s|%s' "$CLAUDE_CONFIG_DIR" "${CLAUDE_SECURESTORAGE_CONFIG_DIR-unset}" > "$dir/$s.env"
sleep 60
"#;

struct Host {
    workers: Workers,
    dir: PathBuf,
    socket: String,
}

impl Drop for Host {
    fn drop(&mut self) {
        self.workers.tmux.kill_server();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn host(name: &str, program: &str) -> Host {
    let dir = std::env::temp_dir().join(format!("worker-host-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let fake = dir.join("claude");
    fs::write(&fake, format!("{PRELUDE}{program}")).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let socket = format!("worker-host-test-{}-{name}", std::process::id());
    Host {
        workers: workers_at(&dir, &socket, &fake.to_string_lossy()),
        dir,
        socket,
    }
}

/// The host as it would be on starting again: nothing but the folders and
/// tmux to go on.
fn workers_at(dir: &Path, socket: &str, program: &str) -> Workers {
    Workers::new(
        Tmux::new(socket),
        Claude::new(program, None),
        dir.join("workers"),
    )
}

impl Host {
    fn home(&self, name: &str) -> Home {
        Home::new(self.dir.join("workers").join(name))
    }

    fn create(&self, id: &str, name: &str) {
        self.workers.create(id, name).unwrap();
    }

    /// The stand-in's own record of `session`, once it has written it.
    fn fake_file(&self, name: &str, session: &str, ext: &str) -> String {
        let file = self.home(name).claude().join(format!(
            "projects/-fake/{}.{ext}",
            claude::session_id(session)
        ));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !file.exists() {
            assert!(
                Instant::now() < deadline,
                "never started: {}",
                file.display()
            );
            thread::sleep(Duration::from_millis(50));
        }
        thread::sleep(Duration::from_millis(100));
        fs::read_to_string(file).unwrap().trim().to_string()
    }

    fn state(&self, name: &str, waiting: bool) -> (State, Option<String>) {
        let home = self.home(name);
        let id = home.record().unwrap().id;
        let live = self.workers.live();
        self.workers
            .state(&home, live.get(&id).map(String::as_str), waiting)
    }
}

/// Who said what, which is all most tests look at.
type Seen = Vec<(&'static str, String)>;

/// Reads on from `cursor` until `done` holds of everything read since the
/// worker's first session began, checking that each line follows the one
/// before it.
fn read_until(
    workers: &Workers,
    home: &Home,
    seen: &mut Seen,
    cursor: &mut Cursor,
    done: impl Fn(&Seen) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done(seen) {
        assert!(Instant::now() < deadline, "gave up; read {seen:?}");
        if let Some(batch) = workers.read(home, *cursor, 1 << 16) {
            take(seen, cursor, batch);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn take(seen: &mut Seen, cursor: &mut Cursor, batch: Batch) {
    for line in batch.lines {
        assert_eq!(line.seq, seen.len() as u64, "{line:?}");
        seen.push((line.from, line.text));
    }
    *cursor = batch.next;
    assert_eq!(cursor.seq, seen.len() as u64);
}

/// Reads on from `cursor` until a line says `text`, and hands back every
/// line read on the way.
fn lines_until(workers: &Workers, home: &Home, cursor: &mut Cursor, text: &str) -> Vec<Line> {
    let mut lines: Vec<Line> = vec![];
    let deadline = Instant::now() + Duration::from_secs(10);
    while !lines.iter().any(|l| l.text == text) {
        assert!(Instant::now() < deadline, "never said {text:?}: {lines:?}");
        if let Some(batch) = workers.read(home, *cursor, 1 << 16) {
            *cursor = batch.next;
            lines.extend(batch.lines);
        }
        thread::sleep(Duration::from_millis(50));
    }
    lines
}

fn told(text: &str) -> (&'static str, String) {
    ("instruction", text.into())
}

fn said(text: &str) -> (&'static str, String) {
    ("agent", text.into())
}

fn command(json: &str) -> Command {
    serde_json::from_str(json).unwrap()
}

fn tmux_names(socket: &str) -> Vec<String> {
    let out = Process::new("tmux")
        .args(["-L", socket, "list-sessions", "-F", "#{session_name}"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn a_worker_is_spoken_to_while_it_works_and_answered_in_the_same_session() {
    let host = host("session", FAKE_CLAUDE);
    let workers = &host.workers;
    host.create("w1", "ana");
    workers
        .start("w1", "s1", false, Some("fix the build"))
        .unwrap();
    assert_eq!(workers.live().into_keys().collect::<Vec<_>>(), ["w1"]);
    let home = host.home("ana");
    let (mut seen, mut cursor) = (vec![], Cursor::default());

    // Read while it is still working, before it has said anything final.
    read_until(workers, &home, &mut seen, &mut cursor, |s| {
        s.contains(&said("step one"))
    });
    assert_eq!(seen, [told("fix the build"), said("step one")]);
    assert!(!cursor.waiting, "mid-turn");
    assert_eq!(
        host.state("ana", cursor.waiting),
        (State::Working, Some("s1".into()))
    );

    workers.say("w1", "use the release branch").unwrap();
    read_until(workers, &home, &mut seen, &mut cursor, |s| {
        s.contains(&said("which branch?"))
    });
    assert!(cursor.waiting, "its turn ended on a question");
    assert_eq!(
        host.state("ana", cursor.waiting),
        (State::Waiting, Some("s1".into()))
    );
    assert_eq!(
        seen[2..],
        [told("use the release branch"), said("which branch?")],
        "the message reached it before it stopped"
    );

    // Answering continues the session: same tmux session, same transcript.
    workers.say("w1", "main").unwrap();
    read_until(workers, &home, &mut seen, &mut cursor, |s| {
        s.contains(&said("on main"))
    });
    assert_eq!(seen[4..], [told("main"), said("on main")]);
    assert_eq!(workers.live().len(), 1);
}

#[test]
fn create_and_start_commands_run_a_worker_in_its_own_folder_and_session() {
    let host = host("start", FAKE_CLAUDE);
    let mut deliveries = Deliveries::new(PATIENCE);
    for json in [
        r#"{"id":"c1","kind":"create","workerId":"w1","name":"ana"}"#,
        r#"{"id":"c2","kind":"start","workerId":"w1","sessionId":"s1","resume":false}"#,
    ] {
        let report = commands::take(&command(json), &host.workers, &mut deliveries).unwrap();
        assert!(report.ok, "{report:?}");
    }
    assert_eq!(tmux_names(&host.socket), ["worker-ana"]);
    let folder = host.dir.join("workers/ana");
    assert_eq!(host.fake_file("ana", "s1", "cwd"), folder.to_string_lossy());
    let home = host.home("ana");
    let (mut seen, mut cursor) = (vec![], Cursor::default());
    read_until(&host.workers, &home, &mut seen, &mut cursor, |s| {
        s.contains(&said("step one"))
    });
    assert_eq!(
        seen[0],
        told(NOTHING_YET),
        "started with nothing to do but wait"
    );
    let rules = fs::read_to_string(home.claude().join("CLAUDE.md")).unwrap();
    assert!(rules.contains("You are worker `ana`"), "{rules}");
}

/// Records what it was started with: its environment, and the context's
/// skill and rules as they stood when it began.
const CONTEXT_CLAUDE: &str = r#"
printf '%s' "$DS_URL" > "$dir/$s.env"
cat "$CLAUDE_CONFIG_DIR/skills/tracker/SKILL.md" > "$dir/$s.skill" 2>/dev/null || echo none > "$dir/$s.skill"
tail -n 1 "$CLAUDE_CONFIG_DIR/CLAUDE.md" > "$dir/$s.rules"
while IFS= read -r line; do :; done
"#;

#[test]
fn a_start_writes_its_context_before_claude_code_starts_and_sets_its_variables() {
    let host = host("context", CONTEXT_CLAUDE);
    let mut deliveries = Deliveries::new(PATIENCE);
    let start = |session: &str, version: u64, skill: &str| {
        format!(
            r#"{{"id":"c-{session}","kind":"start","workerId":"w1","sessionId":"{session}","resume":false,
            "context":{{"key":"global","version":{version},"files":[
              {{"path":"CLAUDE.md","content":"Give feedback before you finish.","executable":false}},
              {{"path":"skills/tracker/SKILL.md","content":"{skill}","executable":false}}]}},
            "env":{{"DS_URL":"https://ds.example","CLAUDE_CONFIG_DIR":"/elsewhere"}}}}"#
        )
    };
    let take = |json: &str, deliveries: &mut Deliveries| {
        let report = commands::take(&command(json), &host.workers, deliveries).unwrap();
        assert!(report.ok, "{report:?}");
    };
    take(
        r#"{"id":"c1","kind":"create","workerId":"w1","name":"ana"}"#,
        &mut deliveries,
    );
    take(&start("s1", 1, "Use wt."), &mut deliveries);
    assert_eq!(host.fake_file("ana", "s1", "env"), "https://ds.example");
    assert_eq!(host.fake_file("ana", "s1", "skill"), "Use wt.");
    assert_eq!(
        host.fake_file("ana", "s1", "rules"),
        "Give feedback before you finish."
    );
    let rules = fs::read_to_string(host.home("ana").claude().join("CLAUDE.md")).unwrap();
    assert!(
        rules.starts_with("You are worker `ana`"),
        "the host's rules come first: {rules}"
    );

    host.workers.stop("w1").unwrap();
    take(&start("s2", 2, "Use wt, then docs."), &mut deliveries);
    assert_eq!(host.fake_file("ana", "s2", "skill"), "Use wt, then docs.");
}

#[test]
fn a_worker_reports_idle_working_waiting_interrupted_and_idle_again() {
    let host = host("states", CHAT_CLAUDE);
    let workers = &host.workers;
    host.create("w1", "ana");
    assert_eq!(
        host.state("ana", false),
        (State::Idle, None),
        "a home, no session"
    );

    workers.start("w1", "s1", false, Some("hello")).unwrap();
    assert_eq!(
        host.state("ana", false),
        (State::Working, Some("s1".into()))
    );
    let home = host.home("ana");
    let (mut seen, mut cursor) = (vec![], Cursor::default());
    read_until(workers, &home, &mut seen, &mut cursor, |s| {
        s.contains(&said("on hello"))
    });
    assert_eq!(
        host.state("ana", cursor.waiting),
        (State::Waiting, Some("s1".into()))
    );

    // The machine goes down, or Claude Code dies: no stop was asked for.
    workers.tmux.kill_server();
    assert_eq!(
        host.state("ana", cursor.waiting),
        (State::Interrupted, Some("s1".into()))
    );
    let restarted = workers_at(&host.dir, &host.socket, "/nonexistent");
    assert_eq!(
        restarted.reconcile(),
        [("ana".to_string(), State::Interrupted)]
    );

    // Stopped, it is idle, and a start of the same session is not run again.
    workers.stop("w1").unwrap();
    assert_eq!(host.state("ana", false), (State::Idle, None));
    workers.start("w1", "s1", false, Some("hello")).unwrap();
    assert!(
        workers.live().is_empty(),
        "started a session that had ended"
    );
}

#[test]
fn a_session_carried_on_writes_on_in_the_same_transcript_under_its_own_id() {
    let host = host("resume", CHAT_CLAUDE);
    let workers = &host.workers;
    host.create("w1", "ana");
    let home = host.home("ana");
    workers.start("w1", "s1", false, Some("one")).unwrap();
    let mut cursor = Cursor::default();
    lines_until(workers, &home, &mut cursor, "on one");
    workers.tmux.kill_server();

    workers.start("w1", "s2", true, Some("two")).unwrap();
    let lines = lines_until(workers, &home, &mut cursor, "on two");
    assert_eq!(
        lines
            .iter()
            .map(|l| (l.seq, l.session_id.as_str(), l.text.as_str()))
            .collect::<Vec<_>>(),
        [(2, "s2", "two"), (3, "s2", "on two")],
        "numbered on from the last session, and its own"
    );
    let transcripts = fs::read_dir(home.claude().join("projects/-fake"))
        .unwrap()
        .filter(|e| e.as_ref().unwrap().path().extension() == Some("jsonl".as_ref()))
        .count();
    assert_eq!(transcripts, 1, "one conversation, carried on");

    // A new conversation is a new transcript, numbered on all the same.
    workers.stop("w1").unwrap();
    workers.start("w1", "s3", false, Some("three")).unwrap();
    let lines = lines_until(workers, &home, &mut cursor, "on three");
    assert_eq!(
        lines
            .iter()
            .map(|l| (l.seq, l.session_id.as_str()))
            .collect::<Vec<_>>(),
        [(4, "s3"), (5, "s3")]
    );

    // A host that restarts reads it all again, and says the same.
    let again = workers.read(&home, Cursor::default(), 1 << 16).unwrap();
    assert_eq!(
        again
            .lines
            .iter()
            .map(|l| (l.seq, l.session_id.as_str(), l.text.as_str()))
            .collect::<Vec<_>>(),
        [
            (0, "s1", "one"),
            (1, "s1", "on one"),
            (2, "s2", "two"),
            (3, "s2", "on two"),
            (4, "s3", "three"),
            (5, "s3", "on three"),
        ]
    );
    assert_eq!(again.next, cursor);
}

#[test]
fn a_resume_with_nothing_said_is_told_to_carry_on() {
    let host = host("carry-on", CHAT_CLAUDE);
    host.create("w1", "ana");
    host.workers.start("w1", "s1", false, Some("one")).unwrap();
    host.fake_file("ana", "s1", "cwd");
    host.workers.stop("w1").unwrap();
    host.workers.start("w1", "s2", true, None).unwrap();
    let home = host.home("ana");
    let (mut seen, mut cursor) = (vec![], Cursor::default());
    read_until(&host.workers, &home, &mut seen, &mut cursor, |s| {
        s.contains(&told(CARRY_ON))
    });
}

#[test]
fn an_interrupt_stops_the_turn_and_the_session_goes_on() {
    let host = host("interrupt", BUSY_CLAUDE);
    let workers = &host.workers;
    host.create("w1", "ana");
    workers.start("w1", "s1", false, None).unwrap();
    let home = host.home("ana");
    let (mut seen, mut cursor) = (vec![], Cursor::default());
    read_until(workers, &home, &mut seen, &mut cursor, |s| {
        s.contains(&said("working"))
    });

    workers.interrupt("w1", Some("try the other way")).unwrap();
    read_until(workers, &home, &mut seen, &mut cursor, |s| {
        s.contains(&said("on try the other way"))
    });
    assert_eq!(
        seen,
        [
            said("working"),
            told("[Request interrupted by user]"),
            told("try the other way"),
            said("on try the other way"),
        ]
    );
    assert_eq!(
        workers.live().len(),
        1,
        "the session outlives the interrupt"
    );
}

#[test]
fn a_message_clears_what_was_left_in_the_input_first() {
    // Claude Code puts an interrupted prompt back in its input; the terminal
    // stand-in holds typed but unsent words the same way.
    let host = host("leftover", CHAT_CLAUDE);
    let workers = &host.workers;
    host.create("w1", "ana");
    workers.start("w1", "s1", false, Some("begin")).unwrap();
    let home = host.home("ana");
    let (mut seen, mut cursor) = (vec![], Cursor::default());
    read_until(workers, &home, &mut seen, &mut cursor, |s| {
        s.contains(&said("on begin"))
    });

    let session = workers.live().remove("w1").unwrap();
    Process::new("tmux")
        .args([
            "-L",
            &host.socket,
            "send-keys",
            "-t",
            &session,
            "-l",
            "stale words ",
        ])
        .status()
        .unwrap();
    workers.say("w1", "next").unwrap();
    read_until(workers, &home, &mut seen, &mut cursor, |s| {
        s.contains(&said("on next"))
    });
    assert!(!seen.contains(&told("stale words next")), "{seen:?}");
}

/// Settles `deliveries` until nothing is held, and says how each ended.
fn settle_all(workers: &Workers, deliveries: &mut Deliveries) -> Vec<(String, Result<(), String>)> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut settled = vec![];
    while !deliveries.is_empty() {
        assert!(Instant::now() < deadline, "never settled");
        settled.extend(deliveries.settle(workers));
        thread::sleep(Duration::from_millis(50));
    }
    settled
}

#[test]
fn a_message_taken_with_its_workers_start_waits_until_it_listens() {
    let host = host("deaf", DEAF_CLAUDE);
    let workers = &host.workers;
    host.create("w1", "ana");
    let mut deliveries = Deliveries::new(PATIENCE);
    // Two commands pushed together, taken one after the other.
    let start = command(r#"{"id":"c1","kind":"start","workerId":"w1","sessionId":"s1"}"#);
    let message = command(r#"{"id":"c2","kind":"message","workerId":"w1","text":"hello"}"#);
    let started = commands::take(&start, workers, &mut deliveries).unwrap();
    assert!(started.ok, "{started:?}");
    assert_eq!(
        commands::take(&message, workers, &mut deliveries),
        None,
        "not reported while it cannot yet be heard"
    );
    assert_eq!(
        commands::take(&message, workers, &mut deliveries),
        None,
        "sent again by the service, it is not typed twice"
    );

    assert_eq!(
        settle_all(workers, &mut deliveries),
        [("c2".to_string(), Ok(()))]
    );
    let home = host.home("ana");
    let (mut seen, mut cursor) = (vec![], Cursor::default());
    read_until(workers, &home, &mut seen, &mut cursor, |s| {
        s.contains(&said("on hello"))
    });
    assert_eq!(seen.iter().filter(|e| **e == told("hello")).count(), 1);
}

#[test]
fn a_message_is_done_only_once_it_is_in_the_transcript() {
    let host = host("delivered", FAKE_CLAUDE);
    let workers = &host.workers;
    host.create("w1", "ana");
    workers
        .start("w1", "s1", false, Some("fix the build"))
        .unwrap();
    let home = host.home("ana");
    let (mut seen, mut cursor) = (vec![], Cursor::default());
    read_until(workers, &home, &mut seen, &mut cursor, |s| {
        s.contains(&said("step one"))
    });

    // Typed at once, but taken only at its next step, a second from now.
    let mut deliveries = Deliveries::new(PATIENCE);
    assert_eq!(deliveries.hold(workers, "c1", "w1", "use main"), None);
    assert_eq!(deliveries.settle(workers), []);
    let end = workers.end("w1");
    assert!(!workers.told_since("w1", 0, "use main"), "not there yet");

    assert_eq!(
        settle_all(workers, &mut deliveries),
        [("c1".to_string(), Ok(()))]
    );
    assert!(
        workers.told_since("w1", end, "use main"),
        "in the transcript when it was reported"
    );
}

#[test]
fn a_message_that_cannot_be_delivered_in_time_fails_with_why() {
    let patience = Duration::from_millis(300);

    let stuck = host("stuck", STUCK_CLAUDE);
    stuck.create("w1", "ana");
    stuck.workers.start("w1", "s1", false, None).unwrap();
    let mut deliveries = Deliveries::new(patience);
    assert_eq!(deliveries.hold(&stuck.workers, "c1", "w1", "hello"), None);
    let [(_, Err(why))] = &settle_all(&stuck.workers, &mut deliveries)[..] else {
        panic!("expected one failure")
    };
    assert!(why.contains("never typed"), "{why}");

    let away = host("away", AWAY_CLAUDE);
    away.create("w1", "ana");
    away.workers.start("w1", "s1", false, None).unwrap();
    let home = away.home("ana");
    let (mut seen, mut cursor) = (vec![], Cursor::default());
    read_until(&away.workers, &home, &mut seen, &mut cursor, |s| {
        !s.is_empty()
    });
    let mut deliveries = Deliveries::new(patience);
    assert_eq!(deliveries.hold(&away.workers, "c1", "w1", "hello"), None);
    let [(_, Err(why))] = &settle_all(&away.workers, &mut deliveries)[..] else {
        panic!("expected one failure")
    };
    assert!(why.contains("not in its transcript"), "{why}");
}

#[test]
fn a_host_that_restarts_picks_up_live_sessions_and_does_not_start_them_again() {
    let host = host("again", FAKE_CLAUDE);
    host.create("w1", "ana");
    host.workers
        .start("w1", "s1", false, Some("fix the build"))
        .unwrap();
    let restarted = workers_at(
        &host.dir,
        &host.socket,
        &host.dir.join("claude").to_string_lossy(),
    );
    assert_eq!(restarted.reconcile(), [("ana".to_string(), State::Working)]);
    restarted
        .start("w1", "s1", false, Some("fix the build"))
        .unwrap();
    assert_eq!(restarted.live().len(), 1);
    let err = restarted.start("w1", "s2", true, None).unwrap_err();
    assert!(err.contains("already has session s1 running"), "{err}");
    assert_eq!(host.home("ana").sessions().len(), 1);
}

#[test]
fn a_start_that_cannot_run_says_why_and_can_be_tried_again() {
    let host = host("missing", FAKE_CLAUDE);
    host.create("w1", "ana");
    let workers = workers_at(&host.dir, &host.socket, "/nonexistent/claude");
    let err = workers.start("w1", "s1", false, None).unwrap_err();
    assert!(
        err.contains("/nonexistent/claude is not installed"),
        "{err}"
    );
    assert!(workers.live().is_empty());
    assert_eq!(host.home("ana").sessions(), []);
    host.workers.start("w1", "s1", false, None).unwrap();
    assert_eq!(host.workers.live().len(), 1, "tried again, it runs");
}

#[test]
fn speaking_to_a_worker_with_no_session_says_so() {
    let host = host("absent", FAKE_CLAUDE);
    host.create("w1", "ana");
    for err in [
        host.workers.interrupt("w1", None).unwrap_err(),
        host.workers.say("w1", "hello").unwrap_err(),
    ] {
        assert!(err.contains("worker w1 has no live session"), "{err}");
    }
    let err = host.workers.start("w9", "s1", false, None).unwrap_err();
    assert!(err.contains("no worker w9 on this host"), "{err}");
}

#[test]
fn a_session_logs_everything_its_terminal_shows() {
    let host = host("logs", CHAT_CLAUDE);
    host.create("w1", "ana");
    host.workers
        .start("w1", "s1", false, Some("hello"))
        .unwrap();
    let log = host.home("ana").log("s1");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fs::read_to_string(&log).is_ok_and(|t| t.contains("said: on hello")) {
        assert!(
            Instant::now() < deadline,
            "nothing logged at {}",
            log.display()
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// A transcript for a session begun for worker `ana`, made if need be.
fn transcript(host: &Host, session: &str) -> PathBuf {
    let home = host.home("ana");
    if home.record().is_none() {
        host.create("w1", "ana");
    }
    let claude = claude::session_id(session);
    home.begin(&Session {
        session_id: session.into(),
        claude: claude.clone(),
        from: 0,
        stopped: false,
    })
    .unwrap();
    let dir = home.claude().join("projects/-x");
    fs::create_dir_all(&dir).unwrap();
    dir.join(format!("{claude}.jsonl"))
}

fn append(path: &PathBuf, bytes: &[u8]) {
    use std::io::Write;
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

#[test]
fn a_read_stops_at_its_budget_and_the_next_picks_up_there() {
    let host = host("budget", FAKE_CLAUDE);
    let line = |text: &str| {
        format!(r#"{{"type":"user","message":{{"role":"user","content":"{text}"}}}}"#) + "\n"
    };
    let body = line("one") + &line("two") + &line("three") + r#"{"type":"user","mess"#;
    fs::write(transcript(&host, "s1"), &body).unwrap();
    let one = Entry::Told { text: "one".into() }.line(0, sys::now(), "s1".into());
    let one = serde_json::to_string(&one).unwrap().len();
    let home = host.home("ana");

    let (mut seen, mut cursor) = (vec![], Cursor::default());
    let first = host.workers.read(&home, cursor, one).unwrap();
    take(&mut seen, &mut cursor, first);
    assert_eq!(seen, [told("one")]);
    let rest = host.workers.read(&home, cursor, 1 << 16).unwrap();
    take(&mut seen, &mut cursor, rest);
    assert_eq!(seen[1..], [told("two"), told("three")]);
    assert_eq!(
        cursor.offset as usize,
        body.rfind('\n').unwrap() + 1,
        "a line still being written is left for later"
    );
    assert!(host.workers.read(&home, cursor, 1 << 16).is_none());
}

#[test]
fn a_line_an_ended_transcript_left_torn_is_passed_over() {
    let host = host("torn", FAKE_CLAUDE);
    let first = transcript(&host, "s1");
    append(
        &first,
        b"{\"type\":\"user\",\"message\":{\"content\":\"one\"}}\n{\"type\":\"user\",\"mes",
    );
    let second = transcript(&host, "s2");
    append(
        &second,
        b"{\"type\":\"user\",\"message\":{\"content\":\"two\"}}\n",
    );
    let batch = host
        .workers
        .read(&host.home("ana"), Cursor::default(), 1 << 16)
        .unwrap();
    assert_eq!(
        batch
            .lines
            .iter()
            .map(|l| (l.seq, l.session_id.as_str(), l.text.as_str()))
            .collect::<Vec<_>>(),
        [(0, "s1", "one"), (1, "s2", "two")]
    );
    assert_eq!(batch.next.file, 1);
}

#[test]
fn a_worker_whose_turn_ended_waits_until_it_is_told_something() {
    let host = host("turns", FAKE_CLAUDE);
    let path = transcript(&host, "s1");
    let home = host.home("ana");
    let cursor = std::cell::Cell::new(Cursor::default());
    let read = |lines: &[&str]| {
        for line in lines {
            append(&path, format!("{line}\n").as_bytes());
        }
        cursor.set(
            host.workers
                .read(&home, cursor.get(), 1 << 16)
                .unwrap()
                .next,
        );
        cursor.get().waiting
    };
    assert!(
        !read(&[
            r#"{"type":"user","message":{"role":"user","content":"fix the build"}}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","name":"Bash","input":{}}],"stop_reason":"tool_use"}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}"#,
        ]),
        "mid-turn"
    );
    assert!(
        read(&[
            r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"thinking","thinking":""}],"stop_reason":null}}"#,
            r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"Which branch?"}],"stop_reason":"end_turn"}}"#,
            r#"{"type":"system","subtype":"turn_duration","durationMs":1}"#,
        ]),
        "its turn ended and nothing has been said since"
    );
    assert!(
        host.workers.read(&home, cursor.get(), 1 << 16).is_none(),
        "nothing new: the report keeps what the cursor says"
    );
    assert!(
        !read(&[r#"{"type":"user","message":{"role":"user","content":"main"}}"#]),
        "told something, it is at work again"
    );
}

// The shape of a real Claude Code 2.1 transcript, which opens with lines of
// its own bookkeeping before the first turn.
#[test]
fn turns_written_after_lines_that_say_nothing_are_read() {
    let host = host("bookkeeping", FAKE_CLAUDE);
    let path = transcript(&host, "s1");
    let home = host.home("ana");
    let opening = concat!(
        r#"{"type":"last-prompt","lastPrompt":"Say hello."}"#,
        "\n",
        r#"{"type":"mode","mode":"normal"}"#,
        "\n",
        r#"{"type":"permission-mode","permissionMode":"bypassPermissions"}"#,
        "\n",
        r#"{"type":"file-history-snapshot","snapshot":{}}"#,
        "\n",
    );
    append(&path, opening.as_bytes());

    let mut cursor = Cursor::default();
    let nothing = host.workers.read(&home, cursor, 1 << 16).unwrap();
    assert_eq!(nothing.lines, []);
    assert_eq!(
        nothing.next,
        Cursor {
            file: 0,
            offset: opening.len() as u64,
            seq: 0,
            waiting: false
        },
        "read to the end of the last whole line, and found nothing to say"
    );
    cursor = nothing.next;

    append(&path, concat!(
        r#"{"type":"user","timestamp":"2026-10-27T13:34:52.123Z","message":{"role":"user","content":"Say hello."}}"#, "\n",
        r#"{"type":"a-kind-not-yet-invented","timestamp":"2026-10-27T13:34:53.000Z"}"#, "\n",
    ).as_bytes());
    append(&path, b"{\"type\":\"attachment\",\"text\":\"\xff\xfe\"}\n");
    append(&path, concat!(
        r#"{"type":"assistant","timestamp":"2026-10-27T13:34:54.5Z","message":{"id":"m1","content":[{"type":"thinking","thinking":""}],"stop_reason":"end_turn"}}"#, "\n",
        r#"{"type":"assistant","timestamp":"yesterday","message":{"id":"m1","content":[{"type":"text","text":"Hello."}],"stop_reason":"end_turn"}}"#, "\n",
        r#"{"type":"assistant","timestamp":"2026-10-27T13:34:55Z","message":{"id":"m2","content":[{"type":"tool_use","name":"Bash","input":{"command":"pwd"}}]}}"#, "\n",
    ).as_bytes());

    let batch = host.workers.read(&home, cursor, 1 << 16).unwrap();
    let read: Vec<_> = batch
        .lines
        .iter()
        .map(|l| (l.seq, l.from, l.text.as_str()))
        .collect();
    assert_eq!(
        read,
        [
            (0, "instruction", "Say hello."),
            (1, "agent", "Hello."),
            (2, "tool", r#"Bash {"command":"pwd"}"#),
        ],
        "past a kind it does not know and a line that is not UTF-8"
    );
    assert_eq!(batch.lines[0].at, "2026-10-27T13:34:52.123Z");
    assert!(
        instant(&batch.lines[1].at),
        "{} is not an instant",
        batch.lines[1].at
    );
    assert_ne!(batch.lines[1].at, "yesterday");
    assert_eq!(batch.lines[2].at, "2026-10-27T13:34:55Z");
    assert_eq!(batch.next.offset, fs::metadata(&path).unwrap().len());
}

#[test]
fn only_an_instant_in_utc_is_taken_as_one() {
    for at in ["2026-10-27T13:34:52.123Z", "2026-10-27T13:34:52Z"] {
        assert!(instant(at), "{at}");
    }
    for at in [
        "",
        "yesterday",
        "2026-10-27T13:34:52",
        "2026-10-27T13:34:52+01:00",
        "2026-10-27T13:34:52.Z",
        "2026-10-27 13:34:52Z",
    ] {
        assert!(!instant(at), "{at}");
    }
    assert!(instant(&sys::now()));
}

#[test]
fn a_create_that_cannot_be_done_says_why() {
    let host = host("uncreated", FAKE_CLAUDE);
    host.create("w2", "bo");
    for (id, name, why) in [
        ("w3", "../up", "not a worker name"),
        ("w3", ".retired", "not a worker name"),
        ("w9", "bo", "already holds worker w2"),
        ("w2", "cy", "is here already, named bo"),
    ] {
        let err = host.workers.create(id, name).unwrap_err();
        assert!(err.contains(why), "{name}: {err}");
    }
}

#[test]
fn a_stop_ends_the_session_and_leaves_its_work_where_it_is() {
    let host = host("stop", FAKE_CLAUDE);
    let workers = &host.workers;
    host.create("w1", "ana");
    workers.start("w1", "s1", false, None).unwrap();
    let work = host.home("ana").dir.join("WORK");
    fs::write(&work, "half-done\n").unwrap();
    let stop = command(r#"{"id":"c1","kind":"stop","workerId":"w1"}"#);
    let mut deliveries = Deliveries::new(PATIENCE);

    let report = commands::take(&stop, workers, &mut deliveries).unwrap();
    assert!(report.ok, "{report:?}");
    assert!(workers.live().is_empty(), "no session is left");
    assert_eq!(host.state("ana", false), (State::Idle, None));
    assert_eq!(fs::read_to_string(&work).unwrap(), "half-done\n");
    let again = commands::take(&stop, workers, &mut deliveries).unwrap();
    assert!(again.ok, "stopping one not running is done: {again:?}");
}

#[test]
fn a_retired_worker_is_kept_aside_and_its_name_can_be_given_again() {
    let host = host("retire", CHAT_CLAUDE);
    let workers = &host.workers;
    host.create("w1", "ana");
    workers.start("w1", "s1", false, Some("hello")).unwrap();
    fs::write(host.home("ana").dir.join("WORK"), "kept\n").unwrap();

    let mut deliveries = Deliveries::new(PATIENCE);
    let retire = command(r#"{"id":"c1","kind":"retire","workerId":"w1"}"#);
    let report = commands::take(&retire, workers, &mut deliveries).unwrap();
    assert!(report.ok, "{report:?}");
    assert!(workers.live().is_empty(), "its session was stopped");
    assert!(workers.all().is_empty(), "no longer reported");
    let aside = host.dir.join("workers/.retired/ana-w1");
    assert_eq!(fs::read_to_string(aside.join("WORK")).unwrap(), "kept\n");
    let again = commands::take(&retire, workers, &mut deliveries).unwrap();
    assert!(again.ok, "retiring one retired is done: {again:?}");
    let err = workers.start("w1", "s2", true, None).unwrap_err();
    assert!(err.contains("worker w1 is retired"), "{err}");
    let err = workers.create("w1", "ana").unwrap_err();
    assert!(err.contains("retired here"), "{err}");

    host.create("w2", "ana");
    assert!(
        !host.home("ana").dir.join("WORK").exists(),
        "a new worker, a new start"
    );
}

#[test]
fn a_brief_too_long_for_tmux_reaches_the_worker_whole() {
    let host = host("long", FAKE_CLAUDE);
    host.create("w1", "ana");
    let long = "x".repeat(60 << 10) + "\nthe end";
    host.workers.start("w1", "s1", false, Some(&long)).unwrap();
    let home = host.home("ana");
    let (mut seen, mut cursor) = (vec![], Cursor::default());
    read_until(&host.workers, &home, &mut seen, &mut cursor, |s| {
        !s.is_empty()
    });
    assert_eq!(seen[0], told("the end"));
    host.workers.stop("w1").unwrap();
    let err = host
        .workers
        .start("w1", "s2", false, Some(&"x".repeat(LONGEST_PROMPT + 1)))
        .unwrap_err();
    assert!(err.contains("more than"), "{err}");
}

#[test]
fn a_session_has_its_own_state_and_the_users_login() {
    let host = host("env", ENV_CLAUDE);
    host.create("w1", "ana");
    host.workers.start("w1", "s1", false, None).unwrap();
    let state = host.home("ana").claude();
    assert_eq!(
        host.fake_file("ana", "s1", "env"),
        format!("{}|", state.display()),
        "its own state, the default login"
    );
}

/// A worker for a test against Claude Code itself, in a folder and on a tmux
/// server of the test's own, with this user's own login, as every worker has.
fn real(name: &str) -> (Workers, Home) {
    let dir = std::env::temp_dir().join(format!("worker-host-real-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let workers = Workers::new(
        Tmux::new(format!(
            "worker-host-test-real-{}-{name}",
            std::process::id()
        )),
        Claude::from_env(|k| std::env::var(k).ok()),
        dir.clone(),
    );
    workers.create(name, name).unwrap();
    (workers, Home::new(dir.join(name)))
}

/// Claude Code itself, spoken to mid-run, then carried on in a second
/// session that remembers the first. It costs real sessions:
/// `cargo test -- --ignored real_claude`.
#[test]
#[ignore]
fn real_claude() {
    let (workers, home) = real("real");
    workers
        .start(
            "real",
            "00000000-0000-4000-8000-000000000001",
            false,
            Some(
                "Run this shell command in the foreground: python3 -c \"import time; \
                 time.sleep(15)\". Then run: echo second. Then reply with one word: done.",
            ),
        )
        .unwrap();
    let (mut seen, mut cursor): (Seen, _) = (vec![], Cursor::default());
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut said = false;
    let ran_third = |s: &Seen| {
        s.iter()
            .position(|(from, text)| *from == "tool" && text.contains("third"))
    };
    while !ran_third(&seen).is_some_and(|i| seen[i..].iter().any(|(from, _)| *from == "agent")) {
        assert!(Instant::now() < deadline, "gave up; read {seen:?}");
        if !said && seen.iter().any(|(from, _)| *from == "tool") {
            workers
                .say("real", "Correction: echo third instead of second.")
                .unwrap();
            said = true;
        }
        if let Some(batch) = workers.read(&home, cursor, 1 << 16) {
            take(&mut seen, &mut cursor, batch);
        }
        thread::sleep(Duration::from_millis(500));
    }
    workers.stop("real").unwrap();
    workers
        .start(
            "real",
            "00000000-0000-4000-8000-000000000002",
            true,
            Some("Which word did I ask you to echo instead of second? Reply with that one word."),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    let mark = seen.len();
    while !seen[mark..]
        .iter()
        .any(|(from, text)| *from == "agent" && text.to_lowercase().contains("third"))
    {
        assert!(
            Instant::now() < deadline,
            "it did not carry on; read {seen:?}"
        );
        if let Some(batch) = workers.read(&home, cursor, 1 << 16) {
            take(&mut seen, &mut cursor, batch);
        }
        thread::sleep(Duration::from_millis(500));
    }
    workers.tmux.kill_server();
    eprintln!("{seen:#?}");
}

/// Interrupting Claude Code itself, mid-command, as the page does:
/// `cargo test -- --ignored real_claude`.
#[test]
#[ignore]
fn real_claude_interrupted() {
    let (workers, home) = real("real-i");
    workers
        .start(
            "real-i",
            "00000000-0000-4000-8000-000000000003",
            false,
            Some(
                "Run this shell command in the foreground: python3 -c \"import time; \
                 time.sleep(90)\". Then reply with one word: slept.",
            ),
        )
        .unwrap();
    let (mut seen, mut cursor): (Seen, _) = (vec![], Cursor::default());
    let started = Instant::now();
    let mut interrupted = false;
    let after = told("Never mind. Reply with one word: stopped.");
    let answered = |s: &Seen| {
        s.iter()
            .position(|e| e == &after)
            .is_some_and(|i| s[i..].iter().any(|(from, _)| *from == "agent"))
    };
    while !answered(&seen) {
        // Well inside the 90 seconds the command runs for, so it was cut off,
        // not waited out.
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "gave up; read {seen:?}"
        );
        if !interrupted && seen.iter().any(|(from, _)| *from == "tool") {
            thread::sleep(Duration::from_secs(3));
            workers
                .interrupt("real-i", Some("Never mind. Reply with one word: stopped."))
                .unwrap();
            interrupted = true;
        }
        if let Some(batch) = workers.read(&home, cursor, 1 << 16) {
            take(&mut seen, &mut cursor, batch);
        }
        thread::sleep(Duration::from_millis(500));
    }
    let live = workers.live().len();
    workers.tmux.kill_server();
    eprintln!("{seen:#?}");
    assert_eq!(live, 1, "the session outlives the interrupt");
    assert!(
        seen.iter()
            .any(|(from, text)| *from == "instruction" && text.starts_with("[Request interrupted")),
        "the turn was stopped"
    );
}
