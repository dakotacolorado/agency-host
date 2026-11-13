use super::*;
use crate::commands;
use crate::config::Permissions;
use crate::protocol::{Command, CommandReport};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{fs, thread};

/// What every stand-in for Claude Code starts with: it takes its arguments as
/// the host gives them -- headless over stream-json, a new session by id or
/// one resumed -- and keeps its own records in the agent's state, where
/// Claude Code would keep its transcript. `next` reads a line from stdin
/// into `$line`, and a message's text into `$text`, echoing the message back
/// as taken; `said` and `ended` write a reply and a turn's end on stdout.
const PRELUDE: &str = r#"#!/bin/sh
dir="$CLAUDE_CONFIG_DIR/projects/-fake"; mkdir -p "$dir"
[ "$1 $2 $3 $4 $5 $6 $7 $8 $9" = "-p --input-format stream-json --output-format stream-json --verbose --replay-user-messages --permission-prompt-tool stdio" ] || exit 64
shift 9
case "$1" in
  --session-id) how=new ;;
  --resume) how=resume ;;
  *) exit 64 ;;
esac
s=$2; t="$dir/$s.jsonl"
printf '%s %s\n' "$how" "$s" >> "$dir/begun"
printf '%s' "$$" > "$dir/$s.pid"
pwd > "$dir/$s.cwd"
e() { printf '%s\n' "$1"; }
said() { e "{\"type\":\"assistant\",\"message\":{\"id\":\"m\",\"content\":[{\"type\":\"text\",\"text\":\"$1\"}],\"stop_reason\":null},\"parent_tool_use_id\":null}"; }
ended() { e '{"type":"result","subtype":"success","is_error":false}'; }
next() {
  IFS= read -r line || exit 0
  touch "$t"
  text=$(printf '%s' "$line" | sed -n 's/^{"message":{"content":"\(.*\)","role":"user"},.*/\1/p')
  if [ -n "$text" ]; then printf '%s\n' "$line" | sed 's/^{/{"isReplay":true,/'; fi
}
"#;

/// Works one step, then takes what was said while it worked -- its next
/// step -- and goes on; after that it answers each message.
const FAKE_CLAUDE: &str = r#"
next
said "step one"
sleep 1
next
said "which branch?"; ended
while next; do said "on $text"; ended; done
"#;

/// Answers each message, its opening one too, and ends each turn.
const CHAT_CLAUDE: &str = r#"
while next; do
  [ -n "$text" ] || continue
  said "on $text"; ended
  echo "said: on $text" >&2
done
"#;

/// A turn that runs until it is stopped: it gives up its turn only when
/// interrupted, and then answers each message.
const BUSY_CLAUDE: &str = r#"
next
said "working"
next
case "$line" in *'"subtype":"interrupt"'*) ;; *) exit 65 ;; esac
e '{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]},"parent_tool_use_id":null}'
e '{"type":"result","subtype":"error_during_execution","is_error":true}'
while next; do said "on $text"; ended; done
"#;

/// As Claude Code starts: nothing is read for a while.
const SLOW_CLAUDE: &str = r#"
sleep 1
while next; do said "on $text"; ended; done
"#;

/// Answers once and then never reads what is said to it.
const AWAY_CLAUDE: &str = r#"
next
said "busy"
sleep 60
"#;

/// Answers once, then ends by itself.
const LEAVING_CLAUDE: &str = r#"
next
said "bye"
sleep 1
"#;

/// Never reads its opening message, so Claude Code writes no transcript.
const STUCK_CLAUDE: &str = "sleep 60\n";

/// Asks to run two tools and keeps the host's answers.
const ASKING_CLAUDE: &str = r#"
next
e '{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}'
next; printf '%s\n' "$line" > "$dir/$s.r1"
e '{"type":"control_request","request_id":"r2","request":{"subtype":"can_use_tool","tool_name":"WebFetch","input":{"url":"https://x"}}}'
next; printf '%s\n' "$line" > "$dir/$s.r2"
said "asked"; ended
while next; do :; done
"#;

/// Records Claude Code's part of its environment, then waits.
const ENV_CLAUDE: &str = r#"
printf '%s|%s' "$CLAUDE_CONFIG_DIR" "${CLAUDE_SECURESTORAGE_CONFIG_DIR-unset}" > "$dir/$s.env"
while next; do :; done
"#;

/// Records how its opening message ends, then waits.
const LONG_CLAUDE: &str = r#"
next
printf '%s' "$text" | tail -c 7 > "$dir/$s.end"
while next; do :; done
"#;

struct Host {
    agents: Agents,
    dir: PathBuf,
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn host(name: &str, program: &str) -> Host {
    host_with(name, program, Permissions::default())
}

fn host_with(name: &str, program: &str, permissions: Permissions) -> Host {
    let dir = std::env::temp_dir().join(format!("agent-host-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let fake = dir.join("claude");
    fs::write(&fake, format!("{PRELUDE}{program}")).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    Host {
        agents: Agents::new(
            Sessions::new(permissions),
            Claude::new(fake.to_string_lossy(), None),
            dir.join("agents"),
        ),
        dir,
    }
}

/// The host as it would be on starting again: nothing but the folders to go
/// on.
fn agents_at(dir: &Path, program: &str) -> Agents {
    Agents::new(
        Sessions::new(Permissions::default()),
        Claude::new(program, None),
        dir.join("agents"),
    )
}

fn wait_for(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        thread::sleep(Duration::from_millis(50));
    }
}

impl Host {
    fn home(&self, name: &str) -> Home {
        Home::new(self.dir.join("agents").join(name))
    }

    fn create(&self, id: &str, name: &str) {
        self.agents.create(id, name).unwrap();
    }

    fn restarted(&self) -> Agents {
        agents_at(&self.dir, &self.dir.join("claude").to_string_lossy())
    }

    fn fake(&self, name: &str) -> PathBuf {
        self.home(name).claude().join("projects/-fake")
    }

    /// The stand-in's own record of the Claude Code session `claude`, once it
    /// has written it.
    fn fake_file(&self, name: &str, claude: &str, ext: &str) -> String {
        let file = self.fake(name).join(format!("{claude}.{ext}"));
        wait_for(&format!("never written: {}", file.display()), || {
            file.exists()
        });
        thread::sleep(Duration::from_millis(100));
        fs::read_to_string(file).unwrap().trim().to_string()
    }

    /// How each Claude Code session was begun, in order, once `n` have.
    fn begun(&self, name: &str, n: usize) -> Vec<String> {
        let read = || -> Vec<String> {
            fs::read_to_string(self.fake(name).join("begun"))
                .unwrap_or_default()
                .lines()
                .map(String::from)
                .collect()
        };
        wait_for(&format!("{n} never begun: {:?}", read()), || {
            read().len() >= n
        });
        thread::sleep(Duration::from_millis(100));
        read()
    }

    /// Ends the stand-in for `claude` as the machine going down would, with
    /// the host none the wiser.
    fn kill(&self, name: &str, claude: &str) {
        let pid: u32 = self.fake_file(name, claude, "pid").parse().unwrap();
        sys::signal_group(pid, sys::SIGKILL);
        thread::sleep(Duration::from_millis(200));
    }

    fn state(&self, name: &str, waiting: bool) -> (State, Option<String>) {
        let home = self.home(name);
        let id = home.record().unwrap().id;
        let live = self.agents.live();
        self.agents.state(&home, live.contains_key(&id), waiting)
    }
}

/// Who said what, which is all most tests look at.
type Seen = Vec<(&'static str, String)>;

/// Reads on from `cursor` until `done` holds of everything read since the
/// agent's first session began, checking that each line follows the one
/// before it.
fn read_until(
    agents: &Agents,
    home: &Home,
    seen: &mut Seen,
    cursor: &mut Cursor,
    done: impl Fn(&Seen) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done(seen) {
        assert!(Instant::now() < deadline, "gave up; read {seen:?}");
        if let Some(batch) = agents.read(home, *cursor, 1 << 16) {
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
fn lines_until(agents: &Agents, home: &Home, cursor: &mut Cursor, text: &str) -> Vec<Line> {
    let mut lines: Vec<Line> = vec![];
    let deadline = Instant::now() + Duration::from_secs(10);
    while !lines.iter().any(|l| l.text == text) {
        assert!(Instant::now() < deadline, "never said {text:?}: {lines:?}");
        if let Some(batch) = agents.read(home, *cursor, 1 << 16) {
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

#[test]
fn an_agent_is_spoken_to_while_it_works_and_answered_in_the_same_session() {
    let host = host("session", FAKE_CLAUDE);
    let agents = &host.agents;
    host.create("w1", "ana");
    agents
        .start("w1", "s1", false, Some("fix the build"))
        .unwrap();
    assert_eq!(agents.live().into_keys().collect::<Vec<_>>(), ["w1"]);
    let home = host.home("ana");
    let (mut seen, mut cursor) = (vec![], Cursor::default());

    // Read while it is still working, before it has said anything final.
    read_until(agents, &home, &mut seen, &mut cursor, |s| {
        s.contains(&said("step one"))
    });
    assert_eq!(seen, [told("fix the build"), said("step one")]);
    assert!(!cursor.waiting, "mid-turn");
    assert_eq!(
        host.state("ana", cursor.waiting),
        (State::Working, Some("s1".into()))
    );

    agents.say("w1", "use the release branch", "u1").unwrap();
    read_until(agents, &home, &mut seen, &mut cursor, |s| {
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

    // Answering continues the session: the same process, the same events.
    agents.say("w1", "main", "u2").unwrap();
    read_until(agents, &home, &mut seen, &mut cursor, |s| {
        s.contains(&said("on main"))
    });
    assert_eq!(seen[4..], [told("main"), said("on main")]);
    assert_eq!(agents.live().len(), 1);
    assert_eq!(host.begun("ana", 1).len(), 1);
}

#[test]
fn create_and_start_commands_run_an_agent_in_its_own_folder_and_session() {
    let host = host("start", FAKE_CLAUDE);
    let mut deliveries = Deliveries::default();
    for json in [
        r#"{"id":"c1","kind":"create","agentId":"w1","name":"ana"}"#,
        r#"{"id":"c2","kind":"start","agentId":"w1","sessionId":"s1","resume":false}"#,
    ] {
        let report = commands::take(&command(json), &host.agents, &mut deliveries).unwrap();
        assert!(report.ok, "{report:?}");
    }
    assert_eq!(
        host.agents.live(),
        [("w1".to_string(), "s1".to_string())].into()
    );
    let folder = host.dir.join("agents/ana");
    let claude = claude::uuid("s1");
    assert_eq!(
        host.fake_file("ana", &claude, "cwd"),
        folder.to_string_lossy()
    );
    assert_eq!(host.begun("ana", 1), [format!("new {claude}")]);
    let home = host.home("ana");
    let (mut seen, mut cursor) = (vec![], Cursor::default());
    read_until(&host.agents, &home, &mut seen, &mut cursor, |s| {
        s.contains(&said("step one"))
    });
    assert_eq!(
        seen[0],
        told(NOTHING_YET),
        "started with nothing to do but wait"
    );
    let rules = fs::read_to_string(home.claude().join("CLAUDE.md")).unwrap();
    assert!(rules.contains("You are agent `ana`"), "{rules}");
}

/// Records what it was started with: its environment, and the context's
/// skill and rules as they stood when it began.
const CONTEXT_CLAUDE: &str = r#"
printf '%s' "$DS_URL" > "$dir/$s.env"
cat "$CLAUDE_CONFIG_DIR/skills/tracker/SKILL.md" > "$dir/$s.skill" 2>/dev/null || echo none > "$dir/$s.skill"
tail -n 1 "$CLAUDE_CONFIG_DIR/CLAUDE.md" > "$dir/$s.rules"
while next; do :; done
"#;

#[test]
fn a_start_writes_its_context_before_claude_code_starts_and_sets_its_variables() {
    let host = host("context", CONTEXT_CLAUDE);
    let mut deliveries = Deliveries::default();
    let start = |session: &str, version: u64, skill: &str| {
        format!(
            r#"{{"id":"c-{session}","kind":"start","agentId":"w1","sessionId":"{session}","resume":false,
            "context":{{"key":"global","version":{version},"files":[
              {{"path":"CLAUDE.md","content":"Give feedback before you finish.","executable":false}},
              {{"path":"skills/tracker/SKILL.md","content":"{skill}","executable":false}}]}},
            "env":{{"DS_URL":"https://ds.example","CLAUDE_CONFIG_DIR":"/elsewhere"}}}}"#
        )
    };
    let take = |json: &str, deliveries: &mut Deliveries| {
        let report = commands::take(&command(json), &host.agents, deliveries).unwrap();
        assert!(report.ok, "{report:?}");
    };
    take(
        r#"{"id":"c1","kind":"create","agentId":"w1","name":"ana"}"#,
        &mut deliveries,
    );
    take(&start("s1", 1, "Use wt."), &mut deliveries);
    let s1 = claude::uuid("s1");
    assert_eq!(host.fake_file("ana", &s1, "env"), "https://ds.example");
    assert_eq!(host.fake_file("ana", &s1, "skill"), "Use wt.");
    assert_eq!(
        host.fake_file("ana", &s1, "rules"),
        "Give feedback before you finish."
    );
    let rules = fs::read_to_string(host.home("ana").claude().join("CLAUDE.md")).unwrap();
    assert!(
        rules.starts_with("You are agent `ana`"),
        "the host's rules come first: {rules}"
    );

    host.agents.stop("w1").unwrap();
    take(&start("s2", 2, "Use wt, then docs."), &mut deliveries);
    assert_eq!(
        host.fake_file("ana", &claude::uuid("s2"), "skill"),
        "Use wt, then docs."
    );
}

#[test]
fn a_start_writes_its_documents_after_its_context_and_their_rules_after_its_own() {
    let host = host("documents", CONTEXT_CLAUDE);
    let mut deliveries = Deliveries::default();
    let take = |json: &str, deliveries: &mut Deliveries| {
        let report = commands::take(&command(json), &host.agents, deliveries).unwrap();
        assert!(report.ok, "{report:?}");
    };
    take(
        r#"{"id":"c1","kind":"create","agentId":"w1","name":"ana"}"#,
        &mut deliveries,
    );
    take(
        r#"{"id":"c2","kind":"start","agentId":"w1","sessionId":"s1","resume":false,
        "context":{"key":"global","version":4,"files":[
          {"path":"CLAUDE.md","content":"Shared by every agent.","executable":false},
          {"path":"skills/tracker/SKILL.md","content":"Use wt.","executable":false}]},
        "documents":[{"key":"approval","version":2,"files":[
          {"path":"CLAUDE.md","content":"Approve or reject the PR.","executable":false},
          {"path":"skills/approve/SKILL.md","content":"How to approve.","executable":false}]}]}"#,
        &mut deliveries,
    );
    assert_eq!(
        host.fake_file("ana", &claude::uuid("s1"), "rules"),
        "Approve or reject the PR."
    );
    let state = host.home("ana").claude();
    let rules = fs::read_to_string(state.join("CLAUDE.md")).unwrap();
    assert!(
        rules.contains("Shared by every agent.\nApprove or reject the PR."),
        "{rules}"
    );
    assert_eq!(
        fs::read_to_string(state.join("skills/approve/SKILL.md")).unwrap(),
        "How to approve."
    );
    let manifest = fs::read_to_string(state.join(".context.json")).unwrap();
    let first = |field: &str| {
        manifest
            .lines()
            .find(|l| l.trim_start().starts_with(&format!("\"{field}\"")))
            .unwrap()
            .to_string()
    };
    assert!(first("key").contains("global") && first("version").contains('4'));
    assert!(manifest.contains("\"approval\""), "{manifest}");
}

/// Records the tokens, the address and the arguments it was started with,
/// then waits.
const SECRET_CLAUDE: &str = r#"
printf '%s|%s' "$WT_TOKEN" "$WT_URL" > "$dir/$s.env"
ps -o args= -p $$ > "$dir/$s.args"
while next; do :; done
"#;

#[test]
fn a_start_gives_its_session_tokens_from_a_file_only_the_host_can_read_and_never_on_a_command_line()
{
    let host = host("secrets", SECRET_CLAUDE);
    let mut deliveries = Deliveries::default();
    let take = |json: &str, deliveries: &mut Deliveries| {
        let report = commands::take(&command(json), &host.agents, deliveries).unwrap();
        assert!(report.ok, "{report:?}");
    };
    take(
        r#"{"id":"c1","kind":"create","agentId":"w1","name":"ana"}"#,
        &mut deliveries,
    );
    take(
        r#"{"id":"c2","kind":"start","agentId":"w1","sessionId":"s1","resume":false,
        "env":{"WT_URL":"https://wt.example"},"credentials":["work-tracker"],
        "secrets":{"WT_TOKEN":"it's $ecret `x`"}}"#,
        &mut deliveries,
    );
    let s1 = claude::uuid("s1");
    assert_eq!(
        host.fake_file("ana", &s1, "env"),
        "it's $ecret `x`|https://wt.example"
    );
    let args = host.fake_file("ana", &s1, "args");
    assert!(
        !args.contains("$ecret"),
        "a token on its command line: {args}"
    );
    let file = host.home("ana").credentials();
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        context::written_credentials(&file),
        [
            ("WT_TOKEN".to_string(), "it's $ecret `x`".to_string()),
            ("WT_URL".to_string(), "https://wt.example".to_string()),
        ]
        .into(),
        "kept for a session resumed after the host restarts"
    );

    host.agents.stop("w1").unwrap();
    take(
        r#"{"id":"c3","kind":"start","agentId":"w1","sessionId":"s2","resume":true}"#,
        &mut deliveries,
    );
    assert!(!file.exists(), "a start without tokens leaves none behind");
}

#[test]
fn a_start_naming_mcp_servers_writes_them_beside_its_records_and_passes_them_to_each_run() {
    let host = host("mcp", SECRET_CLAUDE);
    let mut deliveries = Deliveries::default();
    let take = |json: &str, deliveries: &mut Deliveries| {
        let report = commands::take(&command(json), &host.agents, deliveries).unwrap();
        assert!(report.ok, "{report:?}");
    };
    take(
        r#"{"id":"c1","kind":"create","agentId":"w1","name":"ana"}"#,
        &mut deliveries,
    );
    take(
        r#"{"id":"c2","kind":"start","agentId":"w1","sessionId":"s1","resume":false,
        "mcpServers":{"waypoint":{"type":"http","url":"${WAYPOINT_URL}/mcp",
        "headers":{"Authorization":"Bearer ${WAYPOINT_TOKEN}"}}}}"#,
        &mut deliveries,
    );
    let file = host.home("ana").mcp();
    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(
        written,
        serde_json::json!({"mcpServers":{"waypoint":{"type":"http","url":"${WAYPOINT_URL}/mcp",
            "headers":{"Authorization":"Bearer ${WAYPOINT_TOKEN}"}}}})
    );
    let args = host.fake_file("ana", &claude::uuid("s1"), "args");
    assert!(
        args.contains(&format!("--mcp-config {}", file.display())),
        "{args}"
    );

    host.agents.stop("w1").unwrap();
    take(
        r#"{"id":"c3","kind":"start","agentId":"w1","sessionId":"s2","resume":true}"#,
        &mut deliveries,
    );
    assert!(
        !file.exists(),
        "a start naming no server leaves none behind"
    );
}

#[test]
fn a_permission_asked_for_is_answered_by_the_hosts_rule() {
    let permissions = Permissions {
        allow: vec!["*".into()],
        deny: vec!["WebFetch".into()],
    };
    let host = host_with("asking", ASKING_CLAUDE, permissions);
    host.create("w1", "ana");
    host.agents.start("w1", "s1", false, None).unwrap();
    let s1 = claude::uuid("s1");
    let answer = |r: &str| -> serde_json::Value {
        serde_json::from_str(&host.fake_file("ana", &s1, r)).unwrap()
    };
    let (allowed, denied) = (answer("r1"), answer("r2"));
    assert_eq!(allowed["type"], "control_response");
    assert_eq!(allowed["response"]["request_id"], "r1");
    assert_eq!(
        allowed["response"]["response"],
        serde_json::json!({"behavior":"allow","updatedInput":{"command":"ls"}})
    );
    assert_eq!(denied["response"]["request_id"], "r2");
    assert_eq!(denied["response"]["response"]["behavior"], "deny");
    let home = host.home("ana");
    let (mut seen, mut cursor) = (vec![], Cursor::default());
    read_until(&host.agents, &home, &mut seen, &mut cursor, |s| {
        s.contains(&said("asked"))
    });
    assert!(cursor.waiting);
}

#[test]
fn an_agent_reports_idle_working_waiting_interrupted_and_idle_again() {
    let host = host("states", CHAT_CLAUDE);
    let agents = &host.agents;
    host.create("w1", "ana");
    assert_eq!(
        host.state("ana", false),
        (State::Idle, None),
        "a home, no session"
    );

    agents.start("w1", "s1", false, Some("hello")).unwrap();
    assert_eq!(
        host.state("ana", false),
        (State::Working, Some("s1".into()))
    );
    let home = host.home("ana");
    let (mut seen, mut cursor) = (vec![], Cursor::default());
    read_until(agents, &home, &mut seen, &mut cursor, |s| {
        s.contains(&said("on hello"))
    });
    assert_eq!(
        host.state("ana", cursor.waiting),
        (State::Waiting, Some("s1".into()))
    );

    // Claude Code dies under the host's eyes: no stop was asked for.
    host.kill("ana", &claude::uuid("s1"));
    assert_eq!(
        host.state("ana", cursor.waiting),
        (State::Interrupted, Some("s1".into()))
    );
    let restarted = host.restarted();
    assert_eq!(
        restarted.reconcile(),
        [("ana".to_string(), State::Interrupted)],
        "a session that ended by itself is not resumed"
    );
    assert!(restarted.live().is_empty());

    // Stopped, it is idle, and a start of the same session is not run again.
    agents.stop("w1").unwrap();
    assert_eq!(host.state("ana", false), (State::Idle, None));
    agents.start("w1", "s1", false, Some("hello")).unwrap();
    assert!(agents.live().is_empty(), "started a session that had ended");
}

#[test]
fn a_host_that_restarts_resumes_each_live_session_and_does_not_start_it_again() {
    let host = host("again", CHAT_CLAUDE);
    host.create("w1", "ana");
    host.create("w2", "bo");
    host.agents.start("w1", "s1", false, Some("hello")).unwrap();
    host.agents.start("w2", "s2", false, Some("hi")).unwrap();
    let home = host.home("ana");
    let mut cursor = Cursor::default();
    lines_until(&host.agents, &home, &mut cursor, "on hello");
    host.agents.stop("w2").unwrap();

    // The host goes, and its sessions with it.
    let claude = claude::uuid("s1");
    host.kill("ana", &claude);
    let restarted = host.restarted();
    let mut states = restarted.reconcile();
    states.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        states,
        [
            ("ana".to_string(), State::Working),
            ("bo".to_string(), State::Idle)
        ]
    );
    assert_eq!(
        host.begun("ana", 2),
        [format!("new {claude}"), format!("resume {claude}")]
    );
    assert_eq!(
        host.begun("bo", 1).len(),
        1,
        "a stopped session stays stopped"
    );
    let lines = lines_until(&restarted, &home, &mut cursor, &format!("on {RESUMED}"));
    assert_eq!(
        lines
            .iter()
            .map(|l| (l.seq, l.session_id.as_str(), l.text.as_str()))
            .collect::<Vec<_>>(),
        [
            (2, "s1", RESUMED),
            (3, "s1", format!("on {RESUMED}").as_str())
        ],
        "told it was resumed, and numbered on"
    );
    assert!(cursor.waiting);

    restarted.start("w1", "s1", false, Some("hello")).unwrap();
    assert_eq!(restarted.live().len(), 1);
    let err = restarted.start("w1", "s9", true, None).unwrap_err();
    assert!(err.contains("already has session s1 running"), "{err}");
    assert_eq!(home.sessions().len(), 1);
}

#[test]
fn a_session_cut_off_before_claude_code_wrote_anything_is_begun_again() {
    let host = host("cut-off", STUCK_CLAUDE);
    host.create("w1", "ana");
    host.agents.start("w1", "s1", false, Some("hello")).unwrap();
    let claude = claude::uuid("s1");
    host.kill("ana", &claude);
    let restarted = host.restarted();
    assert_eq!(restarted.reconcile(), [("ana".to_string(), State::Working)]);
    assert_eq!(
        host.begun("ana", 2),
        [format!("new {claude}"), format!("new {claude}")]
    );
}

/// Cut off before it reads anything the first time it runs; after that,
/// answers each message.
const CUT_OFF_ONCE_CLAUDE: &str = r#"
[ "$(wc -l < "$dir/begun")" -gt 1 ] || sleep 60
while next; do [ -n "$text" ] || continue; said "on $text"; ended; done
"#;

#[test]
fn the_line_of_a_commands_text_names_the_command_and_one_the_host_made_names_none() {
    let host = host("named", CUT_OFF_ONCE_CLAUDE);
    host.create("w1", "ana");
    let mut deliveries = Deliveries::default();
    let start = "00000000-0000-4000-8000-00000000000a";
    let report = commands::take(
        &command(&format!(
            r#"{{"id":"{start}","kind":"start","agentId":"w1","sessionId":"s1","text":"hello"}}"#
        )),
        &host.agents,
        &mut deliveries,
    )
    .unwrap();
    assert!(report.ok, "{report:?}");
    // Cut off before it took its first message, which it is told again.
    host.kill("ana", &claude::uuid("s1"));
    let agents = host.restarted();
    agents.reconcile();
    let home = host.home("ana");
    let mut cursor = Cursor::default();
    let named = |lines: Vec<Line>| -> Vec<(String, Option<String>)> {
        lines
            .into_iter()
            .filter(|l| l.from == "instruction")
            .map(|l| (l.text, l.command_id))
            .collect()
    };
    assert_eq!(
        named(lines_until(&agents, &home, &mut cursor, "on hello")),
        [("hello".into(), Some(start.into()))]
    );

    let interrupt = "00000000-0000-4000-8000-00000000000b";
    agents
        .interrupt("w1", interrupt, Some("stop that"))
        .unwrap();
    assert_eq!(
        named(lines_until(&agents, &home, &mut cursor, "on stop that")),
        [("stop that".into(), Some(interrupt.into()))]
    );

    agents.stop("w1").unwrap();
    agents.start("w1", "s2", true, None).unwrap();
    assert_eq!(
        named(lines_until(
            &agents,
            &home,
            &mut cursor,
            &format!("on {CARRY_ON}")
        )),
        [(CARRY_ON.into(), None)]
    );
}

/// An agent whose session an older release began, as it left it:
/// one session, and Claude Code's own transcript of it with one turn.
fn left_by_an_older_release(host: &Host) -> (Home, String, u64) {
    host.create("w1", "ana");
    let home = host.home("ana");
    let claude = claude::uuid("s1");
    fs::write(
        home.dir.join("sessions.jsonl"),
        format!("{{\"sessionId\":\"s1\",\"claude\":\"{claude}\",\"from\":0}}\n"),
    )
    .unwrap();
    let transcript = host.fake("ana").join(format!("{claude}.jsonl"));
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    let turn = concat!(
        r#"{"type":"user","message":{"role":"user","content":"fix the build"}}"#,
        "\n",
        r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"fixed"}],"stop_reason":"end_turn"}}"#,
        "\n",
    );
    fs::write(&transcript, turn).unwrap();
    (home, claude, turn.len() as u64)
}

/// Writes on in Claude Code's transcript, as a resumed Claude Code does,
/// then answers each message.
const RESUMING_CLAUDE: &str = r#"
[ "$how" = resume ] && printf '%s\n' '{"type":"user","message":{"role":"user","content":"written on in the transcript"}}' >> "$t"
while next; do said "on $text"; ended; done
"#;

#[test]
fn a_session_an_older_release_left_is_resumed_headless_and_read_on_with_no_line_twice() {
    let host = host("older", RESUMING_CLAUDE);
    let (home, claude, end) = left_by_an_older_release(&host);
    let restarted = host.restarted();
    assert_eq!(restarted.reconcile(), [("ana".to_string(), State::Working)]);
    assert_eq!(host.begun("ana", 1), [format!("resume {claude}")]);
    let sessions = home.sessions();
    assert_eq!(sessions.len(), 2, "begun again, headless, under its own id");
    assert_eq!(
        (
            sessions[1].session_id.as_str(),
            sessions[1].stream,
            sessions[1].from
        ),
        ("s1", true, end)
    );
    let mut cursor = Cursor::default();
    let lines = lines_until(&restarted, &home, &mut cursor, &format!("on {RESUMED}"));
    assert_eq!(
        lines
            .iter()
            .map(|l| (l.seq, l.session_id.as_str(), l.text.as_str()))
            .collect::<Vec<_>>(),
        [
            (0, "s1", "fix the build"),
            (1, "s1", "fixed"),
            (2, "s1", RESUMED),
            (3, "s1", format!("on {RESUMED}").as_str()),
        ],
        "its transcript up to where it was carried on, then its events"
    );
    assert!(
        fs::read_to_string(host.fake("ana").join(format!("{claude}.jsonl")))
            .unwrap()
            .contains("written on in the transcript"),
        "Claude Code wrote on in its transcript, which is not read again"
    );
    let again = restarted.read(&home, Cursor::default(), 1 << 16).unwrap();
    assert_eq!(again.lines.len(), 4);
    assert_eq!(again.next, cursor);

    restarted.stop("w1").unwrap();
    assert!(home.sessions().iter().all(|s| s.stopped));
    assert_eq!(restarted.reconcile(), [("ana".into(), State::Idle)]);
}

#[test]
fn a_session_carried_on_writes_on_in_the_same_conversation_under_its_own_id() {
    let host = host("resume", CHAT_CLAUDE);
    let agents = &host.agents;
    host.create("w1", "ana");
    let home = host.home("ana");
    agents.start("w1", "s1", false, Some("one")).unwrap();
    let mut cursor = Cursor::default();
    lines_until(agents, &home, &mut cursor, "on one");
    agents.stop("w1").unwrap();

    agents.start("w1", "s2", true, Some("two")).unwrap();
    let lines = lines_until(agents, &home, &mut cursor, "on two");
    assert_eq!(
        lines
            .iter()
            .map(|l| (l.seq, l.session_id.as_str(), l.text.as_str()))
            .collect::<Vec<_>>(),
        [(2, "s2", "two"), (3, "s2", "on two")],
        "numbered on from the last session, and its own"
    );
    let claude = claude::uuid("s1");
    assert_eq!(
        host.begun("ana", 2),
        [format!("new {claude}"), format!("resume {claude}")],
        "one conversation, carried on"
    );

    // A new conversation is a new Claude Code session, numbered on all the same.
    agents.stop("w1").unwrap();
    agents.start("w1", "s3", false, Some("three")).unwrap();
    let lines = lines_until(agents, &home, &mut cursor, "on three");
    assert_eq!(
        lines
            .iter()
            .map(|l| (l.seq, l.session_id.as_str()))
            .collect::<Vec<_>>(),
        [(4, "s3"), (5, "s3")]
    );

    // A host that restarts reads it all again, and says the same.
    let again = agents.read(&home, Cursor::default(), 1 << 16).unwrap();
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
    host.agents.start("w1", "s1", false, Some("one")).unwrap();
    host.fake_file("ana", &claude::uuid("s1"), "cwd");
    host.agents.stop("w1").unwrap();
    host.agents.start("w1", "s2", true, None).unwrap();
    let home = host.home("ana");
    let (mut seen, mut cursor) = (vec![], Cursor::default());
    read_until(&host.agents, &home, &mut seen, &mut cursor, |s| {
        s.contains(&told(CARRY_ON))
    });
}

#[test]
fn an_interrupt_stops_the_turn_and_the_session_goes_on() {
    let host = host("interrupt", BUSY_CLAUDE);
    let agents = &host.agents;
    host.create("w1", "ana");
    agents.start("w1", "s1", false, None).unwrap();
    let home = host.home("ana");
    let (mut seen, mut cursor) = (vec![], Cursor::default());
    read_until(agents, &home, &mut seen, &mut cursor, |s| {
        s.contains(&said("working"))
    });
    assert!(!cursor.waiting);

    agents
        .interrupt("w1", "c1", Some("try the other way"))
        .unwrap();
    read_until(agents, &home, &mut seen, &mut cursor, |s| {
        s.contains(&said("on try the other way"))
    });
    assert_eq!(
        seen,
        [
            told(NOTHING_YET),
            said("working"),
            told("[Request interrupted by user]"),
            told("try the other way"),
            said("on try the other way"),
        ]
    );
    assert!(cursor.waiting);
    assert_eq!(agents.live().len(), 1, "the session outlives the interrupt");
}

/// Settles `deliveries` until nothing is held, and says how each ended.
fn settle_all(agents: &Agents, deliveries: &mut Deliveries) -> Vec<(String, Result<(), String>)> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut settled = vec![];
    while !deliveries.is_empty() {
        assert!(Instant::now() < deadline, "never settled");
        settled.extend(deliveries.settle(agents));
        thread::sleep(Duration::from_millis(50));
    }
    settled
}

#[test]
fn a_message_taken_with_its_agents_start_is_written_at_once_and_taken_once_claude_code_takes_it() {
    let host = host("slow", SLOW_CLAUDE);
    let agents = &host.agents;
    host.create("w1", "ana");
    let mut deliveries = Deliveries::default();
    let mut taken = commands::Taken::default();
    // Two commands pushed together, taken one after the other.
    let start = command(r#"{"id":"c1","kind":"start","agentId":"w1","sessionId":"s1"}"#);
    let message = command(r#"{"id":"c2","kind":"message","agentId":"w1","text":"hello"}"#);
    let started = taken.take(&start, agents, &mut deliveries).unwrap();
    assert!(started.ok, "{started:?}");
    let written = CommandReport::written("c2".into());
    assert_eq!(
        taken.take(&message, agents, &mut deliveries),
        Some(written.clone()),
        "reported written before Claude Code has taken it"
    );
    assert_eq!(
        taken.take(&message, agents, &mut deliveries),
        Some(written),
        "sent again by the service, it is answered as written and not written twice"
    );
    assert_eq!(
        commands::take(&message, agents, &mut deliveries),
        None,
        "nor while it is held"
    );

    let settled = settle_all(agents, &mut deliveries);
    assert_eq!(settled, [("c2".to_string(), Ok(()))]);
    taken.remember(&CommandReport::new("c2".into(), None));
    assert_eq!(
        taken.take(&message, agents, &mut deliveries),
        Some(CommandReport::new("c2".into(), None)),
        "sent again once taken, it is answered as taken"
    );
    let home = host.home("ana");
    let (mut seen, mut cursor) = (vec![], Cursor::default());
    read_until(agents, &home, &mut seen, &mut cursor, |s| {
        s.contains(&said("on hello"))
    });
    assert_eq!(seen.iter().filter(|e| **e == told("hello")).count(), 1);
}

/// Works on, reading nothing, for longer than a test waits on one settle;
/// then takes what was said meanwhile.
const LATE_CLAUDE: &str = r#"
next
said "busy"
sleep 2
next
said "on $text"; ended
while next; do :; done
"#;

#[test]
fn a_message_is_taken_once_claude_code_echoes_it_however_late() {
    let host = host("delivered", LATE_CLAUDE);
    let agents = &host.agents;
    host.create("w1", "ana");
    agents
        .start("w1", "s1", false, Some("fix the build"))
        .unwrap();
    let home = host.home("ana");
    let (mut seen, mut cursor) = (vec![], Cursor::default());
    read_until(agents, &home, &mut seen, &mut cursor, |s| {
        s.contains(&said("busy"))
    });

    // Written at once, but taken only at its next step, seconds from now.
    let mut deliveries = Deliveries::default();
    let id = "00000000-0000-4000-8000-0000000000c1";
    assert_eq!(deliveries.hold(agents, id, "w1", "use main"), Ok(()));
    let held = Instant::now();
    while held.elapsed() < Duration::from_millis(1200) {
        assert_eq!(deliveries.settle(agents), [], "not taken yet");
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        settle_all(agents, &mut deliveries),
        [(id.to_string(), Ok(()))]
    );

    let lines = lines_until(agents, &home, &mut cursor, "on use main");
    let told: Vec<_> = lines
        .iter()
        .filter(|l| l.from == "instruction")
        .map(|l| (l.text.as_str(), l.command_id.as_deref()))
        .collect();
    assert_eq!(told, [("use main", Some(id))], "its line names its command");
}

#[test]
fn a_message_never_taken_fails_only_when_its_session_ends() {
    let away = host("away", AWAY_CLAUDE);
    away.create("w1", "ana");
    away.agents.start("w1", "s1", false, None).unwrap();
    let home = away.home("ana");
    let (mut seen, mut cursor) = (vec![], Cursor::default());
    read_until(&away.agents, &home, &mut seen, &mut cursor, |s| {
        s.contains(&said("busy"))
    });
    let mut deliveries = Deliveries::default();
    assert_eq!(deliveries.hold(&away.agents, "c1", "w1", "hello"), Ok(()));
    thread::sleep(Duration::from_millis(500));
    assert_eq!(deliveries.settle(&away.agents), [], "held, however long");
    // Begun again: the new session never had it.
    away.agents.stop("w1").unwrap();
    away.agents.start("w1", "s2", true, None).unwrap();
    let [(_, Err(why))] = &settle_all(&away.agents, &mut deliveries)[..] else {
        panic!("expected one failure")
    };
    assert!(why.contains("ended before it took the message"), "{why}");

    let leaving = host("leaving", LEAVING_CLAUDE);
    leaving.create("w1", "ana");
    leaving.agents.start("w1", "s1", false, None).unwrap();
    let home = leaving.home("ana");
    let (mut seen, mut cursor) = (vec![], Cursor::default());
    read_until(&leaving.agents, &home, &mut seen, &mut cursor, |s| {
        s.contains(&said("bye"))
    });
    let mut deliveries = Deliveries::default();
    assert_eq!(
        deliveries.hold(&leaving.agents, "c1", "w1", "hello"),
        Ok(())
    );
    let [(_, Err(why))] = &settle_all(&leaving.agents, &mut deliveries)[..] else {
        panic!("expected one failure")
    };
    assert!(why.contains("ended before it took the message"), "{why}");
}

/// Takes its opening message and the one after it, then ends by itself.
const TAKE_AND_GO_CLAUDE: &str = r#"
next
next
"#;

#[test]
fn a_message_taken_just_before_its_session_ends_is_taken() {
    let host = host("taken-then-gone", TAKE_AND_GO_CLAUDE);
    host.create("w1", "ana");
    host.agents.start("w1", "s1", false, None).unwrap();
    let mut deliveries = Deliveries::default();
    assert_eq!(deliveries.hold(&host.agents, "c1", "w1", "hello"), Ok(()));
    wait_for("its session ends", || host.agents.live().is_empty());
    assert_eq!(
        settle_all(&host.agents, &mut deliveries),
        [("c1".to_string(), Ok(()))]
    );
}

#[test]
fn a_start_that_cannot_run_says_why_and_can_be_tried_again() {
    let host = host("missing", FAKE_CLAUDE);
    host.create("w1", "ana");
    let agents = agents_at(&host.dir, "/nonexistent/claude");
    let err = agents.start("w1", "s1", false, None).unwrap_err();
    assert!(
        err.contains("/nonexistent/claude is not installed"),
        "{err}"
    );
    assert!(agents.live().is_empty());
    assert_eq!(host.home("ana").sessions(), []);
    host.agents.start("w1", "s1", false, None).unwrap();
    assert_eq!(host.agents.live().len(), 1, "tried again, it runs");
}

#[test]
fn speaking_to_an_agent_with_no_session_says_so() {
    let host = host("absent", FAKE_CLAUDE);
    host.create("w1", "ana");
    for err in [
        host.agents.interrupt("w1", "c1", None).unwrap_err(),
        host.agents.say("w1", "hello", "u1").unwrap_err(),
    ] {
        assert!(err.contains("agent w1 has no live session"), "{err}");
    }
    let err = host.agents.start("w9", "s1", false, None).unwrap_err();
    assert!(err.contains("no agent w9 on this host"), "{err}");
}

#[test]
fn a_session_keeps_what_it_says_on_stdout_and_stderr() {
    let host = host("logs", CHAT_CLAUDE);
    host.create("w1", "ana");
    host.agents.start("w1", "s1", false, Some("hello")).unwrap();
    let home = host.home("ana");
    let (events, errors) = (home.events("s1"), home.errors("s1"));
    wait_for("nothing on stderr kept", || {
        fs::read_to_string(&errors).is_ok_and(|t| t.contains("said: on hello"))
    });
    let kept = fs::read_to_string(&events).unwrap();
    let kept: Vec<serde_json::Value> = kept
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(kept.len(), 4, "{kept:?}");
    assert_eq!(
        kept[0],
        claude::started(kept[0]["timestamp"].as_str().unwrap().into())
    );
    assert_eq!(kept[1]["isReplay"], true);
    assert_eq!(kept[3]["type"], "result");
    assert!(
        kept.iter()
            .all(|e| e["timestamp"].as_str().is_some_and(instant)),
        "each kept with when it came"
    );
}

/// A headless session begun for agent `ana`, made if need be, and where
/// its events are kept.
fn events(host: &Host, session: &str) -> PathBuf {
    let home = host.home("ana");
    if home.record().is_none() {
        host.create("w1", "ana");
    }
    home.begin(&Session {
        session_id: session.into(),
        claude: claude::uuid(session),
        from: 0,
        stream: true,
        opening: None,
        stopped: false,
        ended: false,
    })
    .unwrap();
    home.events(session)
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
fn a_transcript_carried_on_headless_is_read_up_to_where_it_was_then_its_events() {
    let host = host("carried", FAKE_CLAUDE);
    let (home, claude, end) = left_by_an_older_release(&host);
    let transcript = host.fake("ana").join(format!("{claude}.jsonl"));
    home.begin(&Session {
        session_id: "s1".into(),
        claude: claude.clone(),
        from: end,
        stream: true,
        opening: None,
        stopped: false,
        ended: false,
    })
    .unwrap();
    append(
        &transcript,
        b"{\"type\":\"user\",\"message\":{\"content\":\"twice\"}}\n",
    );
    append(
        &home.events("s1"),
        b"{\"type\":\"user\",\"message\":{\"content\":\"twice\"},\"isReplay\":true}\n",
    );
    let batch = host.agents.read(&home, Cursor::default(), 1 << 16).unwrap();
    assert_eq!(
        batch
            .lines
            .iter()
            .map(|l| (l.seq, l.text.as_str()))
            .collect::<Vec<_>>(),
        [(0, "fix the build"), (1, "fixed"), (2, "twice")]
    );
    assert_eq!(batch.next.file, 1);
}

#[test]
fn a_read_stops_at_its_budget_and_the_next_picks_up_there() {
    let host = host("budget", FAKE_CLAUDE);
    let line = |text: &str| {
        format!(r#"{{"type":"user","message":{{"role":"user","content":"{text}"}}}}"#) + "\n"
    };
    let body = line("one") + &line("two") + &line("three") + r#"{"type":"user","mess"#;
    let path = events(&host, "s1");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, &body).unwrap();
    let one = Entry::Told {
        text: "one".into(),
        command: None,
    }
    .line(0, sys::now(), "s1".into());
    let one = serde_json::to_string(&one).unwrap().len();
    let home = host.home("ana");

    let (mut seen, mut cursor) = (vec![], Cursor::default());
    let first = host.agents.read(&home, cursor, one).unwrap();
    take(&mut seen, &mut cursor, first);
    assert_eq!(seen, [told("one")]);
    let rest = host.agents.read(&home, cursor, 1 << 16).unwrap();
    take(&mut seen, &mut cursor, rest);
    assert_eq!(seen[1..], [told("two"), told("three")]);
    assert_eq!(
        cursor.offset as usize,
        body.rfind('\n').unwrap() + 1,
        "a line still being written is left for later"
    );
    assert!(host.agents.read(&home, cursor, 1 << 16).is_none());
}

#[test]
fn a_long_reply_and_the_message_it_answers_are_read_whole() {
    let host = host("whole", FAKE_CLAUDE);
    let path = events(&host, "s1");
    let message = "Read the whole design, then say what you make of it. ".repeat(300);
    let reply = "Here is what I make of it, point by point.\n".repeat(466);
    assert!(reply.chars().count() >= 20_000);
    let too_long = "z".repeat(claude::LONGEST_TEXT + 1);
    for line in [
        serde_json::json!({"type":"user","message":{"role":"user","content":message},"isReplay":true}),
        serde_json::json!({"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":reply}]}}),
        serde_json::json!({"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":too_long}]}}),
    ] {
        append(&path, format!("{line}\n").as_bytes());
    }
    let home = host.home("ana");
    let (mut seen, mut cursor) = (vec![], Cursor::default());
    while let Some(batch) = host.agents.read(&home, cursor, 32 << 10) {
        take(&mut seen, &mut cursor, batch);
    }
    assert_eq!(seen[..2], [told(&message), said(&reply)]);
    let (from, cut) = &seen[2];
    assert_eq!(*from, "agent");
    let note = format!(" of {} characters]", claude::LONGEST_TEXT + 1);
    assert!(
        cut.starts_with("zzz") && cut.ends_with(&note),
        "{}",
        &cut[cut.len() - 40..]
    );
}

#[test]
fn one_line_of_many_long_texts_still_fits_one_report() {
    let host = host("blocks", FAKE_CLAUDE);
    let path = events(&host, "s1");
    let block = "y".repeat(60_000);
    let line = serde_json::json!({"type":"user","message":{"role":"user","content":[
        {"type":"text","text":block},{"type":"text","text":block},{"type":"text","text":block}]}});
    append(&path, format!("{line}\n").as_bytes());
    let batch = host
        .agents
        .read(&host.home("ana"), Cursor::default(), 32 << 10)
        .unwrap();
    let size: usize = batch
        .lines
        .iter()
        .map(|l| serde_json::to_string(l).unwrap().len())
        .sum();
    assert_eq!(batch.lines.len(), 3);
    assert!(size < 100 << 10, "{size} bytes");
    assert_eq!(batch.lines[0].text, block);
    for cut in &batch.lines[1..] {
        assert!(cut.text.ends_with(" of 60000 characters]"), "{}", cut.text);
    }
}

#[test]
fn a_line_an_ended_session_left_torn_is_passed_over() {
    let host = host("torn", FAKE_CLAUDE);
    let first = events(&host, "s1");
    append(
        &first,
        b"{\"type\":\"user\",\"message\":{\"content\":\"one\"}}\n{\"type\":\"user\",\"mes",
    );
    let second = events(&host, "s2");
    append(
        &second,
        b"{\"type\":\"user\",\"message\":{\"content\":\"two\"}}\n",
    );
    let batch = host
        .agents
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
fn an_agent_whose_turn_ended_waits_until_it_is_told_something() {
    let host = host("turns", FAKE_CLAUDE);
    let path = events(&host, "s1");
    let home = host.home("ana");
    let cursor = std::cell::Cell::new(Cursor::default());
    let read = |lines: &[&str]| {
        for line in lines {
            append(&path, format!("{line}\n").as_bytes());
        }
        cursor.set(host.agents.read(&home, cursor.get(), 1 << 16).unwrap().next);
        cursor.get().waiting
    };
    assert!(
        !read(&[
            r#"{"type":"system","subtype":"init","session_id":"s"}"#,
            r#"{"type":"user","message":{"role":"user","content":"fix the build"},"isReplay":true}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","name":"Bash","input":{}}],"stop_reason":null}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}"#,
        ]),
        "mid-turn"
    );
    assert!(
        read(&[
            r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"Which branch?"}],"stop_reason":null}}"#,
            r#"{"type":"result","subtype":"success","is_error":false}"#,
            r#"{"type":"system","subtype":"task_updated"}"#,
        ]),
        "its turn ended and nothing has been said since"
    );
    assert!(
        host.agents.read(&home, cursor.get(), 1 << 16).is_none(),
        "nothing new: the report keeps what the cursor says"
    );
    assert!(
        !read(&[r#"{"type":"system","subtype":"init","session_id":"s"}"#]),
        "a turn begun by itself, as a background task's end begins one, is work"
    );
}

// The events below are cut down from Claude Code 2.1.318's stdout, on this
// host on 2026-11-05: a background Bash a message asked for, the turn its end
// began by itself, and a background subagent.

#[test]
fn an_agent_reports_its_background_tasks_and_whether_a_message_began_its_turn() {
    let host = host("tasks", FAKE_CLAUDE);
    let path = events(&host, "s1");
    let home = host.home("ana");
    let cursor = std::cell::Cell::new(Cursor::default());
    let read = |lines: &[&str]| {
        for line in lines {
            append(&path, format!("{line}\n").as_bytes());
        }
        let next = host.agents.read(&home, cursor.get(), 1 << 16).unwrap().next;
        cursor.set(next);
        (next.waiting, next.tasks, next.told)
    };
    let report = |live| host.agents.report(&home, "w1", live, &cursor.get());

    assert_eq!(
        read(&[&claude::started("2026-11-05T18:13:01.000Z".into()).to_string()]),
        (false, 0, None),
        "no turn yet"
    );
    assert_eq!(report(true).told, None);
    assert_eq!(
        read(&[
            r#"{"type":"system","subtype":"init","session_id":"s","claude_code_version":"2.1.318"}"#,
            r#"{"isReplay":true,"message":{"content":"Run the shell command 'sleep 60' in the background (run_in_background), and once it finishes, tell me what it printed.","role":"user"},"parent_tool_use_id":null,"session_id":"s","timestamp":"2026-11-05T18:13:03.576Z","type":"user","uuid":"6ca57950-c56f-4d90-9d8b-76b18d3abf6b"}"#,
            r#"{"message":{"content":[{"id":"toolu_01XLs3yGmBEDD4v3Yt9oMjT5","input":{"command":"sleep 60","run_in_background":true},"name":"Bash","type":"tool_use"}],"id":"msg_1","role":"assistant","stop_reason":null,"type":"message"},"parent_tool_use_id":null,"session_id":"s","type":"assistant"}"#,
            r#"{"session_id":"s","subtype":"background_tasks_changed","tasks":[{"description":"Sleep 60 seconds then print finish time","run_id":"0mw3v5wh5-b560cdda","task_id":"bhzuwzdd1","task_type":"local_bash"}],"timestamp":"2026-11-05T18:13:06.858Z","type":"system"}"#,
            r#"{"description":"Sleep 60 seconds then print finish time","is_backgrounded":true,"run_id":"0mw3v5wh5-b560cdda","session_id":"s","subtype":"task_started","task_id":"bhzuwzdd1","task_type":"local_bash","tool_use_id":"toolu_01XLs3yGmBEDD4v3Yt9oMjT5","type":"system"}"#,
        ]),
        (false, 1, Some(true)),
        "at work on what it was told, with a command running in the background"
    );
    assert_eq!(
        read(&[
            r#"{"message":{"content":[{"text":"The command is running in the background.","type":"text"}],"id":"msg_2","role":"assistant","stop_reason":null,"type":"message"},"parent_tool_use_id":null,"type":"assistant"}"#,
            r#"{"is_error":false,"subtype":"success","terminal_reason":"completed","type":"result"}"#,
        ]),
        (true, 1, Some(true)),
        "its turn ends; the command runs on"
    );
    let between = report(true);
    assert_eq!(
        (between.state, between.tasks, between.told),
        (State::Waiting, 1, Some(true))
    );

    assert_eq!(
        read(&[
            r#"{"patch":{"end_time":1793902446874,"status":"completed"},"run_id":"0mw3v5wh5-b560cdda","session_id":"s","subtype":"task_updated","task_id":"bhzuwzdd1","type":"system"}"#,
            r#"{"output_file":"/tmp/tasks/bhzuwzdd1.output","run_id":"0mw3v5wh5-b560cdda","session_id":"s","status":"completed","subtype":"task_notification","summary":"Background command \"Sleep 60 seconds then print finish time\" completed (exit code 0)","task_id":"bhzuwzdd1","type":"system"}"#,
            r#"{"session_id":"s","subtype":"background_tasks_changed","tasks":[],"timestamp":"2026-11-05T18:14:06.876Z","type":"system"}"#,
        ]),
        (true, 0, Some(true)),
        "the command ended between turns"
    );
    let ended = report(true);
    assert_eq!(ended.state, State::Waiting);
    assert_ne!(
        ended, between,
        "a change in tasks alone is a change to report"
    );

    assert_eq!(
        read(&[
            r#"{"type":"system","subtype":"init","session_id":"s","claude_code_version":"2.1.318"}"#,
            r#"{"message":{"content":[{"id":"toolu_01MT6tqrpjZ8NidMMWHYrazc","input":{"file_path":"/tmp/tasks/bhzuwzdd1.output"},"name":"Read","type":"tool_use"}],"id":"msg_3","role":"assistant","stop_reason":null,"type":"message"},"parent_tool_use_id":null,"type":"assistant"}"#,
        ]),
        (false, 0, Some(false)),
        "a turn its end began by itself, with nothing said to it"
    );
    assert_eq!(
        read(&[
            r#"{"description":"base-python and template repos","is_backgrounded":true,"session_id":"s","subtype":"task_started","task_id":"a0bfc7e8e2fca49db","task_type":"local_agent","type":"system"}"#,
            r#"{"session_id":"s","subtype":"background_tasks_changed","tasks":[{"description":"base-python and template repos","run_id":"0mw3w0a1b-1c2d3e4f","subagent_type":"general-purpose","task_id":"a0bfc7e8e2fca49db","task_type":"local_agent"}],"type":"system"}"#,
            r#"{"message":{"content":[{"text":"A subagent's own step.","type":"text"}],"id":"msg_4","role":"assistant","stop_reason":null,"type":"message"},"parent_tool_use_id":"toolu_011w6xoYWPeLRinXqRF2jYnm","type":"assistant"}"#,
            r#"{"description":"Read store patterns","is_backgrounded":false,"session_id":"s","subtype":"task_started","task_id":"beous1ami","task_type":"local_bash","type":"system"}"#,
            r#"{"session_id":"s","status":"completed","subtype":"task_notification","task_id":"beous1ami","type":"system"}"#,
        ]),
        (false, 1, Some(false)),
        "a subagent in the background; a foreground Bash is not counted"
    );
    assert_eq!(
        read(&[
            r#"{"session_id":"s","status":"completed","subtype":"task_notification","summary":"Agent \"base-python and template repos\" finished","task_id":"a0bfc7e8e2fca49db","type":"system"}"#,
            r#"{"session_id":"s","subtype":"background_tasks_changed","tasks":[],"type":"system"}"#,
            r#"{"isReplay":true,"message":{"content":"<task-notification>\n<task-id>a0bfc7e8e2fca49db</task-id>\n<status>completed</status>\n</task-notification>","role":"user"},"origin":{"kind":"task-notification","producer":"session-task","runId":"0mw3w0a1b-1c2d3e4f"},"parent_tool_use_id":null,"session_id":"s","type":"user","uuid":"be02f7a2-ee4f-4047-bb46-83e075322311"}"#,
        ]),
        (false, 0, Some(false)),
        "the subagent's notice, replayed mid-turn, is not a message delivered"
    );
    assert_eq!(
        read(&[
            r#"{"isReplay":true,"message":{"content":"okay I think its working","role":"user"},"parent_tool_use_id":null,"session_id":"s","type":"user","uuid":"9ed71137-6f45-49bb-8efe-7b3063f6f51a"}"#,
        ]),
        (false, 0, Some(true)),
        "a message taken mid-turn"
    );

    // The machine goes down with a command running; the host resumes the
    // session, which writes on in the same events.
    read(&[
        r#"{"session_id":"s","subtype":"background_tasks_changed","tasks":[{"description":"cargo build","run_id":"r","task_id":"b1","task_type":"local_bash"}],"type":"system"}"#,
    ]);
    assert_eq!(cursor.get().tasks, 1);
    assert_eq!(report(false).tasks, 0, "a session not running has no tasks");
    assert_eq!(
        read(&[&claude::started("2026-11-05T19:00:00.000Z".into()).to_string()]).1,
        0,
        "the old process's tasks died with it"
    );
}

#[test]
fn a_new_session_starts_with_no_tasks_and_no_turn() {
    let host = host("tasks-next", FAKE_CLAUDE);
    let first = events(&host, "s1");
    append(&first, concat!(
        r#"{"type":"system","subtype":"init","session_id":"s"}"#, "\n",
        r#"{"type":"user","message":{"role":"user","content":"go"},"isReplay":true,"uuid":"u1"}"#, "\n",
        r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"b1","task_type":"local_bash"}]}"#, "\n",
    ).as_bytes());
    let home = host.home("ana");
    let at = host
        .agents
        .read(&home, Cursor::default(), 1 << 16)
        .unwrap()
        .next;
    assert_eq!((at.file, at.tasks, at.told), (0, 1, Some(true)));
    let second = events(&host, "s2");
    append(&second, b"{\"type\":\"rate_limit_event\"}\n");
    let at = host.agents.read(&home, at, 1 << 16).unwrap().next;
    assert_eq!((at.file, at.tasks, at.told), (1, 0, None));
}

#[test]
fn a_session_resumed_after_a_line_left_torn_marks_its_start_on_a_line_of_its_own() {
    let host = host("torn", CHAT_CLAUDE);
    host.create("w1", "ana");
    let home = host.home("ana");
    let events = home.events("s1");
    fs::create_dir_all(events.parent().unwrap()).unwrap();
    fs::write(&events, r#"{"type":"assistant","message":{"#).unwrap();
    host.agents.start("w1", "s1", false, Some("hello")).unwrap();
    wait_for("never answered", || {
        fs::read_to_string(&events).is_ok_and(|t| t.contains("on hello"))
    });
    let kept = fs::read_to_string(&events).unwrap();
    let second: serde_json::Value = serde_json::from_str(kept.lines().nth(1).unwrap()).unwrap();
    assert_eq!(second["subtype"], "process_started", "{kept}");
}

#[test]
fn a_read_carries_the_last_usage_its_lines_said_at_the_time_it_was_said() {
    let host = host("usage", FAKE_CLAUDE);
    let path = events(&host, "s1");
    let home = host.home("ana");
    append(&path, concat!(
        r#"{"type":"rate_limit_event","timestamp":"2026-11-06T08:28:17.268Z","rate_limit_info":{"unifiedWindows":{"five_hour":{"resetsAt":1793965200,"utilization":0.48},"seven_day":{"resetsAt":1794063600,"utilization":0.4}}}}"#, "\n",
        r#"{"type":"rate_limit_event","timestamp":"2026-11-06T08:28:32.206Z","rate_limit_info":{"unifiedWindows":{"five_hour":{"resetsAt":1793965200,"utilization":0.49}}}}"#, "\n",
        r#"{"type":"rate_limit_event","timestamp":"2026-11-06T08:28:40.000Z","rate_limit_info":{"status":"allowed"}}"#, "\n",
    ).as_bytes());
    let batch = host.agents.read(&home, Cursor::default(), 1 << 16).unwrap();
    assert_eq!(batch.lines, []);
    assert_eq!(
        batch.usage,
        Some(Usage {
            read_at: "2026-11-06T08:28:32.206Z".into(),
            windows: crate::protocol::Windows {
                five_hour: Some(crate::protocol::Window {
                    utilization: 0.49,
                    resets_at: "2026-11-06T11:40:00.000Z".into(),
                }),
                seven_day: None,
            },
        })
    );
    append(&path, b"{\"type\":\"result\"}\n");
    let after = host.agents.read(&home, batch.next, 1 << 16).unwrap();
    assert_eq!(after.usage, None, "a reading is read once");
}

fn tokens(input: u64, output: u64, creation: u64, read: u64) -> Tokens {
    Tokens {
        input_tokens: input,
        output_tokens: output,
        cache_creation_input_tokens: creation,
        cache_read_input_tokens: read,
    }
}

/// A `result` as Claude Code 2.1.318 writes one, cut down to its counts:
/// `since` is the turn's, subagents included, `whole` the conversation's.
fn result_line(since: Tokens, main: Tokens, whole: Tokens) -> String {
    let model = |t: Tokens| {
        serde_json::json!({"inputTokens":t.input_tokens,"outputTokens":t.output_tokens,
            "cacheCreationInputTokens":t.cache_creation_input_tokens,"cacheReadInputTokens":t.cache_read_input_tokens})
    };
    let line = serde_json::json!({"type":"result","subtype":"success","is_error":false,
        "modelUsage":{"claude-opus-5-5":model(whole)},
        "model_usage_since_last_result":{"claude-opus-5-5":model(since)},
        "usage":{"input_tokens":main.input_tokens,"output_tokens":main.output_tokens,
            "cache_creation_input_tokens":main.cache_creation_input_tokens,"cache_read_input_tokens":main.cache_read_input_tokens}});
    format!("{line}\n")
}

/// Every line of an agent's sessions read from the start in reads of
/// `budget`, and each session's total as the last read that changed it said.
fn totals_read(agents: &Agents, home: &Home, budget: usize) -> BTreeMap<String, Tokens> {
    let (mut cursor, mut totals) = (Cursor::default(), BTreeMap::new());
    while let Some(batch) = agents.read(home, cursor, budget) {
        totals.extend(batch.tokens);
        cursor = batch.next;
    }
    totals
}

/// A turn of a session on Claude Code 2.1.318, cut down: the main
/// conversation's message and a subagent's, each with the usage it streamed
/// (only the start of its output), then the turn's `result`.
const A_TURN: &str = concat!(
    r#"{"type":"system","subtype":"init","session_id":"c"}"#,
    "\n",
    r#"{"type":"user","message":{"role":"user","content":"Review it."},"parent_tool_use_id":null,"uuid":"u1","isReplay":true}"#,
    "\n",
    r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"Agent","input":{}}],"usage":{"input_tokens":72,"output_tokens":1249,"cache_creation_input_tokens":67953,"cache_read_input_tokens":1825262}},"parent_tool_use_id":null}"#,
    "\n",
    r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"a subagent's step"}],"usage":{"input_tokens":8,"output_tokens":129,"cache_creation_input_tokens":3953,"cache_read_input_tokens":150762}},"parent_tool_use_id":"t1"}"#,
    "\n",
);

#[test]
fn a_sessions_tokens_are_its_results_its_subagents_included_and_each_counted_once() {
    let host = host("tokens", FAKE_CLAUDE);
    let path = events(&host, "s1");
    let home = host.home("ana");
    let turn = tokens(80, 19491, 71906, 1976024);
    append(&path, A_TURN.as_bytes());
    append(
        &path,
        result_line(turn, tokens(72, 18286, 67953, 1825262), turn).as_bytes(),
    );
    let batch = host.agents.read(&home, Cursor::default(), 1 << 16).unwrap();
    assert_eq!(
        batch.tokens,
        [("s1".to_string(), turn)],
        "the turn's, its subagent's included, and neither message's again"
    );
    assert_eq!(batch.next.tokens, turn);

    let second = tokens(30, 7583, 23753, 1158832);
    append(
        &path,
        result_line(second, tokens(6, 368, 2178, 160523), turn.add(second)).as_bytes(),
    );
    let after = host.agents.read(&home, batch.next, 1 << 16).unwrap();
    assert_eq!(after.tokens, [("s1".to_string(), turn.add(second))]);
    append(&path, b"{\"type\":\"system\",\"subtype\":\"status\"}\n");
    let nothing = host.agents.read(&home, after.next, 1 << 16).unwrap();
    assert_eq!(nothing.tokens, [], "only a session whose total changed");
}

#[test]
fn a_session_resumed_in_a_new_process_carries_its_total_on_and_one_carried_on_starts_its_own() {
    let host = host("resumed-tokens", FAKE_CLAUDE);
    let path = events(&host, "s1");
    let home = host.home("ana");
    let (first, again) = (
        tokens(156, 46675, 152123, 7851713),
        tokens(6, 1093, 9349, 505020),
    );
    append(&path, A_TURN.as_bytes());
    append(&path, result_line(first, first, first).as_bytes());
    // The host restarted and resumed s1 in a new Claude Code, which carries
    // the conversation's `modelUsage` on.
    append(
        &path,
        format!("{}\n", claude::started("2026-11-07T06:00:00.000Z".into())).as_bytes(),
    );
    append(&path, A_TURN.as_bytes());
    append(
        &path,
        result_line(again, again, first.add(again)).as_bytes(),
    );
    // s2 carries the conversation on in a session of its own.
    let path = events(&host, "s2");
    let third = tokens(10, 2396, 3246, 870716);
    append(&path, A_TURN.as_bytes());
    append(
        &path,
        result_line(third, third, first.add(again).add(third)).as_bytes(),
    );

    let totals = totals_read(&host.agents, &home, 1 << 16);
    assert_eq!(
        totals,
        BTreeMap::from([
            ("s1".to_string(), first.add(again)),
            ("s2".to_string(), third)
        ])
    );
}

#[test]
fn a_host_that_restarts_and_reads_a_session_again_comes_to_the_same_total() {
    let host = host("reread-tokens", FAKE_CLAUDE);
    let path = events(&host, "s1");
    let home = host.home("ana");
    let mut whole = Tokens::default();
    for n in 1..=5 {
        let turn = tokens(n, 100 * n, 1000 * n, 10_000 * n);
        whole = whole.add(turn);
        append(&path, A_TURN.as_bytes());
        append(&path, result_line(turn, turn, whole).as_bytes());
    }
    let before = totals_read(&host.agents, &home, 1 << 16);
    assert_eq!(before, BTreeMap::from([("s1".to_string(), whole)]));
    let restarted = host.restarted();
    assert_eq!(totals_read(&restarted, &home, 1 << 16), before);
    assert_eq!(
        totals_read(&restarted, &home, 1),
        before,
        "however many reads it takes"
    );
}

#[test]
fn turns_written_after_lines_that_say_nothing_are_read() {
    let host = host("bookkeeping", FAKE_CLAUDE);
    let path = events(&host, "s1");
    let home = host.home("ana");
    let opening = concat!(
        r#"{"type":"system","subtype":"commands_changed","commands":[]}"#,
        "\n",
        r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#,
        "\n",
        r#"{"type":"command_lifecycle","command_uuid":"u","state":"queued"}"#,
        "\n",
    );
    append(&path, opening.as_bytes());

    let mut cursor = Cursor::default();
    let nothing = host.agents.read(&home, cursor, 1 << 16).unwrap();
    assert_eq!(nothing.lines, []);
    assert_eq!(
        nothing.next,
        Cursor {
            file: 0,
            offset: opening.len() as u64,
            seq: 0,
            waiting: false,
            tasks: 0,
            told: None,
            tokens: Tokens::default(),
        },
        "read to the end of the last whole line, and found nothing to say"
    );
    cursor = nothing.next;

    append(&path, concat!(
        r#"{"type":"user","timestamp":"2026-10-27T13:34:52.123Z","message":{"role":"user","content":"Say hello."},"isReplay":true}"#, "\n",
        r#"{"type":"a-kind-not-yet-invented","timestamp":"2026-10-27T13:34:53.000Z"}"#, "\n",
    ).as_bytes());
    append(&path, b"{\"type\":\"attachment\",\"text\":\"\xff\xfe\"}\n");
    append(&path, concat!(
        r#"{"type":"assistant","timestamp":"2026-10-27T13:34:54.5Z","message":{"id":"m1","content":[{"type":"thinking","thinking":""}]}}"#, "\n",
        r#"{"type":"assistant","timestamp":"yesterday","message":{"id":"m1","content":[{"type":"text","text":"Hello."}]}}"#, "\n",
        r#"{"type":"assistant","timestamp":"2026-10-27T13:34:55Z","message":{"id":"m2","content":[{"type":"tool_use","name":"Bash","input":{"command":"pwd"}}]}}"#, "\n",
    ).as_bytes());

    let batch = host.agents.read(&home, cursor, 1 << 16).unwrap();
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
        ("w3", "../up", "not an agent name"),
        ("w3", ".retired", "not an agent name"),
        ("w9", "bo", "already holds agent w2"),
        ("w2", "cy", "is here already, named bo"),
    ] {
        let err = host.agents.create(id, name).unwrap_err();
        assert!(err.contains(why), "{name}: {err}");
    }
}

/// Starts a tool that runs on after it, as a background command does.
const SPAWNING_CLAUDE: &str = r#"
sleep 300 &
printf '%s' "$!" > "$dir/$s.tool"
while next; do :; done
"#;

#[test]
fn a_stop_ends_the_session_and_what_it_started_and_leaves_its_work_where_it_is() {
    let host = host("stop", SPAWNING_CLAUDE);
    let agents = &host.agents;
    host.create("w1", "ana");
    agents.start("w1", "s1", false, None).unwrap();
    let tool = host.fake_file("ana", &claude::uuid("s1"), "tool");
    let work = host.home("ana").dir.join("WORK");
    fs::write(&work, "half-done\n").unwrap();
    let stop = command(r#"{"id":"c1","kind":"stop","agentId":"w1"}"#);
    let mut deliveries = Deliveries::default();

    let report = commands::take(&stop, agents, &mut deliveries).unwrap();
    assert!(report.ok, "{report:?}");
    assert!(agents.live().is_empty(), "no session is left");
    assert!(
        !Path::new(&format!("/proc/{tool}")).exists() || cfg!(not(target_os = "linux")),
        "its tool runs on"
    );
    assert_eq!(host.state("ana", false), (State::Idle, None));
    assert_eq!(fs::read_to_string(&work).unwrap(), "half-done\n");
    let again = commands::take(&stop, agents, &mut deliveries).unwrap();
    assert!(again.ok, "stopping one not running is done: {again:?}");
}

#[test]
fn a_retired_agent_is_kept_aside_and_its_name_can_be_given_again() {
    let host = host("retire", CHAT_CLAUDE);
    let agents = &host.agents;
    host.create("w1", "ana");
    agents.start("w1", "s1", false, Some("hello")).unwrap();
    fs::write(host.home("ana").dir.join("WORK"), "kept\n").unwrap();

    let mut deliveries = Deliveries::default();
    let retire = command(r#"{"id":"c1","kind":"retire","agentId":"w1"}"#);
    let report = commands::take(&retire, agents, &mut deliveries).unwrap();
    assert!(report.ok, "{report:?}");
    assert!(agents.live().is_empty(), "its session was stopped");
    assert!(agents.all().is_empty(), "no longer reported");
    let aside = host.dir.join("agents/.retired/ana-w1");
    assert_eq!(fs::read_to_string(aside.join("WORK")).unwrap(), "kept\n");
    let again = commands::take(&retire, agents, &mut deliveries).unwrap();
    assert!(again.ok, "retiring one retired is done: {again:?}");
    let err = agents.start("w1", "s2", true, None).unwrap_err();
    assert!(err.contains("agent w1 is retired"), "{err}");
    let err = agents.create("w1", "ana").unwrap_err();
    assert!(err.contains("retired here"), "{err}");

    host.create("w2", "ana");
    assert!(
        !host.home("ana").dir.join("WORK").exists(),
        "a new agent, a new start"
    );
}

#[test]
fn a_brief_of_any_length_reaches_the_agent_whole() {
    let host = host("long", LONG_CLAUDE);
    host.create("w1", "ana");
    let long = "x".repeat(256 << 10) + "\nthe end";
    host.agents.start("w1", "s1", false, Some(&long)).unwrap();
    assert_eq!(host.fake_file("ana", &claude::uuid("s1"), "end"), "the end");
}

#[test]
fn a_session_has_its_own_state_and_the_users_login() {
    let host = host("env", ENV_CLAUDE);
    host.create("w1", "ana");
    host.agents.start("w1", "s1", false, None).unwrap();
    let state = host.home("ana").claude();
    assert_eq!(
        host.fake_file("ana", &claude::uuid("s1"), "env"),
        format!("{}|", state.display()),
        "its own state, the default login"
    );
}

/// An agent for a test against Claude Code itself, in a folder of the test's
/// own, with this user's own login, as every agent has.
fn real(name: &str) -> (Agents, Home, PathBuf) {
    let dir = std::env::temp_dir().join(format!("agent-host-real-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let agents = real_at(&dir);
    agents.create(name, name).unwrap();
    (agents, Home::new(dir.join(name)), dir)
}

fn real_at(dir: &Path) -> Agents {
    Agents::new(
        Sessions::new(Permissions::default()),
        Claude::from_env(|k| std::env::var(k).ok(), dir),
        dir.to_path_buf(),
    )
}

/// Reads on until `done`, giving up after `within`.
fn read_real(
    agents: &Agents,
    home: &Home,
    seen: &mut Seen,
    cursor: &mut Cursor,
    within: Duration,
    mut each: impl FnMut(&Seen),
    done: impl Fn(&Seen, &Cursor) -> bool,
) {
    let deadline = Instant::now() + within;
    while !done(seen, cursor) {
        assert!(Instant::now() < deadline, "gave up; read {seen:#?}");
        each(seen);
        if let Some(batch) = agents.read(home, *cursor, 1 << 16) {
            take(seen, cursor, batch);
        }
        thread::sleep(Duration::from_millis(500));
    }
}

/// Claude Code itself, headless: the release this host is checked against,
/// asked permission for its tools, spoken to mid-run, carried on in a second
/// session that remembers the first, and resumed after its host restarts.
/// It costs real sessions: `cargo test -- --ignored real_claude`.
#[test]
#[ignore]
fn real_claude() {
    let (agents, home, dir) = real("real");
    assert_eq!(
        Claude::from_env(|k| std::env::var(k).ok(), Path::new("/nonexistent"))
            .version()
            .as_deref(),
        Some(claude::CHECKED),
        "the Claude Code this host's stream-json is checked against"
    );
    agents
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
    let mut deliveries = Deliveries::default();
    let mut said = false;
    let ran_third = |s: &Seen| {
        s.iter()
            .position(|(from, text)| *from == "tool" && text.contains("third"))
    };
    read_real(
        &agents,
        &home,
        &mut seen,
        &mut cursor,
        Duration::from_secs(120),
        |seen| {
            if !said && seen.iter().any(|(from, _)| *from == "tool") {
                let held = deliveries.hold(
                    &agents,
                    "00000000-0000-4000-8000-0000000000c1",
                    "real",
                    "Correction: echo third instead of second.",
                );
                assert_eq!(held, Ok(()));
                assert_eq!(
                    deliveries.settle(&agents),
                    [],
                    "taken only at its next step"
                );
                said = true;
            }
        },
        |s, _| ran_third(s).is_some_and(|i| s[i..].iter().any(|(from, _)| *from == "agent")),
    );
    assert_eq!(
        settle_all(&agents, &mut deliveries),
        [("00000000-0000-4000-8000-0000000000c1".to_string(), Ok(()))]
    );
    assert_eq!(
        seen.iter()
            .filter(|e| **e == told("Correction: echo third instead of second."))
            .count(),
        1,
        "a message taken mid-turn is one line"
    );
    read_real(
        &agents,
        &home,
        &mut seen,
        &mut cursor,
        Duration::from_secs(60),
        |_| {},
        |_, c| c.waiting,
    );
    agents.stop("real").unwrap();

    agents
        .start(
            "real",
            "00000000-0000-4000-8000-000000000002",
            true,
            Some("Which word did I ask you to echo instead of second? Reply with that one word."),
        )
        .unwrap();
    let mark = seen.len();
    read_real(
        &agents,
        &home,
        &mut seen,
        &mut cursor,
        Duration::from_secs(120),
        |_| {},
        |s, _| {
            s[mark..]
                .iter()
                .any(|(from, text)| *from == "agent" && text.to_lowercase().contains("third"))
        },
    );

    // The host goes, and comes back.
    drop(agents);
    let restarted = real_at(&dir);
    assert_eq!(
        restarted.reconcile(),
        [("real".to_string(), State::Working)]
    );
    let mark = seen.len();
    read_real(
        &restarted,
        &home,
        &mut seen,
        &mut cursor,
        Duration::from_secs(120),
        |_| {},
        |s, c| s[mark..].contains(&told(RESUMED)) && c.waiting,
    );
    restarted
        .say(
            "real",
            "What word did you echo? Reply with that one word.",
            &claude::uuid("real-c2"),
        )
        .unwrap();
    let mark = seen.len();
    read_real(
        &restarted,
        &home,
        &mut seen,
        &mut cursor,
        Duration::from_secs(120),
        |_| {},
        |s, _| {
            s[mark..]
                .iter()
                .any(|(from, text)| *from == "agent" && text.to_lowercase().contains("third"))
        },
    );
    restarted.stop("real").unwrap();
    eprintln!("{seen:#?}");

    let (first, second) = (
        "00000000-0000-4000-8000-000000000001",
        "00000000-0000-4000-8000-000000000002",
    );
    let reported = totals_read(&restarted, &home, 1 << 16);
    let (first_turns, first_whole) = results_said(&home, first);
    let (second_turns, second_whole) = results_said(&home, second);
    eprintln!(
        "reported {reported:#?}\nfirst: turns {first_turns:?}, conversation {first_whole:?}\n\
         second: turns {second_turns:?}, conversation {second_whole:?}"
    );
    assert_eq!(
        reported,
        BTreeMap::from([(first.into(), first_turns), (second.into(), second_turns)]),
        "each session's total is what its results said its turns used"
    );
    assert_eq!(
        first_turns, first_whole,
        "the conversation's first session, in one process, used all it has"
    );
    assert_ne!(second_turns, second_whole, "not the conversation's again");
}

/// What the `result`s in a session's events said: the sum of what each said
/// its turn used, and what the last said the whole conversation had.
fn results_said(home: &Home, session: &str) -> (Tokens, Tokens) {
    let summed = |models: &serde_json::Value| {
        let count = |key: &str| -> u64 {
            models
                .as_object()
                .unwrap()
                .values()
                .map(|m| m[key].as_u64().unwrap())
                .sum()
        };
        tokens(
            count("inputTokens"),
            count("outputTokens"),
            count("cacheCreationInputTokens"),
            count("cacheReadInputTokens"),
        )
    };
    let (mut turns, mut whole) = (Tokens::default(), Tokens::default());
    for line in fs::read_to_string(home.events(session)).unwrap().lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        if v["type"] == "result" {
            turns = turns.add(summed(&v["model_usage_since_last_result"]));
            whole = summed(&v["modelUsage"]);
        }
    }
    (turns, whole)
}

/// Interrupting Claude Code itself, mid-command, as the page does:
/// `cargo test -- --ignored real_claude`.
#[test]
#[ignore]
fn real_claude_interrupted() {
    let (agents, home, _) = real("real-i");
    agents
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
    read_real(
        &agents,
        &home,
        &mut seen,
        &mut cursor,
        // Well inside the 90 seconds the command runs for, so it was cut off,
        // not waited out.
        Duration::from_secs(60),
        |seen| {
            if !interrupted && seen.iter().any(|(from, _)| *from == "tool") {
                thread::sleep(Duration::from_secs(3));
                agents
                    .interrupt(
                        "real-i",
                        "00000000-0000-4000-8000-0000000000c3",
                        Some("Never mind. Reply with one word: stopped."),
                    )
                    .unwrap();
                interrupted = true;
            }
        },
        |s, _| {
            s.iter()
                .position(|e| e == &after)
                .is_some_and(|i| s[i..].iter().any(|(from, _)| *from == "agent"))
        },
    );
    assert!(started.elapsed() < Duration::from_secs(60));
    let live = agents.live().len();
    agents.stop("real-i").unwrap();
    eprintln!("{seen:#?}");
    assert_eq!(live, 1, "the session outlives the interrupt");
    assert!(
        seen.iter()
            .any(|(from, text)| *from == "instruction" && text.starts_with("[Request interrupted")),
        "the turn was stopped"
    );
}
