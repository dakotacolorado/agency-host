# worker-host

One static binary per machine. It sends heartbeats to `worker-service`, which
say only that it is here and what it offers, and keeps a socket open to it:
every command comes down the socket and every report goes up it. It makes
workers' folders, runs their Claude Code sessions headless, carries messages to
them and what they say back.

## Install

On a mac or linux machine with `gh` signed in, and Claude Code installed and
signed in (its stream-json is checked against 2.1.318, and the host says when
it starts if it finds another):

```sh
scripts/install-host.sh https://… <token>
~/.local/bin/worker-host 2>> ~/worker-host.log &   # on linux; a mac's starts itself
```

The script puts the latest release in `~/.local/bin` and, if there is none,
writes the config file below. Workers' shared context comes with each
session's start (see Workers), so nothing else is installed; a run also takes
away the worker-skills clone at `<workers_dir>/.claude` and its cron sync that
earlier versions installed. Run it again for a newer release, then, on linux,
restart the host: each session that was live is resumed (see Restarts).

On a mac the script also makes launchd run the host, as the LaunchAgent
`com.github.dakotacolorado.worker-host`, whose plist in `~/Library/LaunchAgents`
each run writes afresh. launchd starts the host at login and again whenever it
exits, so `kill` restarts it rather than stopping it; it logs to
`~/worker-host.log`. It is started through your shell, interactive and login
(`zsh -ilc`), so it and its sessions have the PATH and variables a terminal
has: change those in your shell's startup files, then restart it. Each run of
the script restarts it onto the new release. The first run stops any host
started by hand and loads the agent in its place; after that, do not start one
by hand, which would run beside it as a second host. To restart it, stop it until the next login, or start it again:

```sh
launchctl kickstart -k gui/$(id -u)/com.github.dakotacolorado.worker-host
launchctl bootout gui/$(id -u)/com.github.dakotacolorado.worker-host
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.github.dakotacolorado.worker-host.plist
```

To stop it for good, `bootout` it and remove the plist.

## Configure

`~/.config/worker-host/config.toml` (or `--config PATH`, or
`WORKER_HOST_CONFIG`) holds every setting:

```toml
service_url = "https://…"
token = "…"
host_id = "studio"                 # default: the hostname
capacity = 2                       # unset, nothing is placed here
labels = ["macos"]
workers_dir = "/Users/me/workers"  # default: ~/workers

[permissions]                      # what a session may run when it asks
allow = ["*"]                      # the default: every tool
deny = ["WebFetch", "mcp__github__merge*"]
```

A session's permission requests come to the host, which answers each by
`[permissions]`: a tool named in `deny` is refused, then one named in `allow`
is given, any other refused; a name ending in `*` is every tool it begins.
Claude Code's own rules in the worker's settings are checked first, and decide
what it never asks about. Sessions no longer run with
`--dangerously-skip-permissions`.

Two hosts on one machine (beta beside prod) each need their own `workers_dir`. `worker-host --version` prints the release it was built as
(e.g. `worker-host v0.1.0-48`), which is also the `version` its heartbeats send,
so `/fleet/hosts` shows which release each host runs. A build not given its
tag in `WORKER_HOST_RELEASE` (`cargo build`, and every release up to
v0.1.0-47) says Cargo.toml's version, `0.1.0`.

## Socket

Commands arrive only on the socket: pushed when written, and every one still
pending pushed again when a socket opens. Reports (workers' states, transcript
lines, command results) go only on it. What has not been written on a socket is
kept, and each socket opened is first sent every worker's state, the lines not
yet sent, and the results not yet sent.

## Workers

A `create` makes `<workers_dir>/<name>/`:

```text
worker.json       id and name
sessions.jsonl    each session started, and each stopped
prompt.md         the opening message of the latest session
credentials.env   the latest session's variables and tokens, readable by the host's user alone
home/.claude/     its own Claude Code (CLAUDE_CONFIG_DIR): settings, CLAUDE.md, transcripts
logs/<id>.jsonl   every event each session wrote on its stdout, as it came
logs/<id>.log     everything each session wrote on its stderr
```

A `start` may carry a context: files written into `home/.claude/` before
Claude Code starts, `skills/...` as files (scripts executable) and a
`CLAUDE.md` appended to the host's own rules, in place of the last context's
(listed in `home/.claude/.context.json`); a start without one keeps the last.
With it may come `documents`, written after it the same way, each `CLAUDE.md`
after the one before. It may also carry variables to set in the session, such
as the services' addresses, and `secrets`, their tokens: both are written to
`credentials.env`, mode 600, and read into the session's environment each time
it starts, so no token is on a command line and a session resumed after a
restart has them again. The host does not know where any of it comes from.

The worker clones whatever code it needs itself. A `start` runs Claude Code
headless as a child of the host, in that folder: `claude -p --input-format
stream-json --output-format stream-json`, a new conversation under
`--session-id` or, with `resume`, the last one under `--resume`. Messages go
in as JSON lines on its stdin, and what it says comes out as events on its
stdout, which the host keeps in `logs/<id>.jsonl` and reports from. Each
session shares the host user's Claude login: HOME stays the user's, and
`CLAUDE_SECURESTORAGE_CONFIG_DIR` points at the user's own store (see
`src/claude.rs`). A worker's settings deny the `AskUserQuestion`,
`EnterPlanMode` and `ExitPlanMode` tools, which wait on a person at a
terminal: a worker asks in words and ends its turn instead.

States: `working`, `waiting` (its last turn ended, nothing said since), `idle`
(no session), `interrupted` (its session ended by itself; can be resumed).
With each state go `tasks`, the background tasks and subagents its live
session has open (from Claude Code's latest `background_tasks_changed`), and
`told`, whether its turn or its last was begun by a message the host delivered
rather than by itself, as one a background task's end begins is. The host
writes a `process_started` line in `logs/<id>.jsonl` before each Claude Code
it runs, so a session resumed there starts with no tasks. A `message` is
written to the session's stdin at once and reported done once Claude Code
echoes it back, as it does when it takes it at its next step; `interrupt` asks
Claude Code to stop the turn, as Escape does; `stop` ends the session and
whatever it started; `retire` stops it and moves the folder to
`<workers_dir>/.retired/<name>-<id>/`. Every command is idempotent: one sent
again is answered, not redone.

To take a session over by hand, stop the worker (so the host does not write
to it too), then in its folder run its Claude Code with its own state:

```sh
CLAUDE_CONFIG_DIR=<workers_dir>/<name>/home/.claude claude --resume <id>
```

where `<id>` is the `claude` of its last line in `sessions.jsonl`. A `start`
with `resume` hands it back to the host, carrying on what was said by hand.

## Restarts

A session does not outlive its host: it is the host's child, and is told to
stop when the host goes (on macOS, when it next writes to it). A host that
starts again resumes each session that was live when it went, with
`claude --resume`, and tells it the host restarted. What costs is the turn in
flight: whatever Claude Code was in the middle of -- a reply, a tool -- is cut
off, and the worker is told to check it. A session that was stopped, or that
ended by itself while the host ran, is left as it was.

Start or restart the host from your own login, not from inside a worker's
session: the host takes its own `CLAUDE_CONFIG_DIR` to be where your Claude
login is. One inside `workers_dir` can only be a worker's, so the host ignores
it, says so in its log, and gives sessions Claude Code's default login.
On a mac, `launchctl` may be run from anywhere, since launchd starts the host
with the agent's environment, not the caller's.

A session an older release began is resumed the same way, and what it said
before is read from Claude Code's own transcript, so its lines are numbered on.
A setting only an older release read is accepted and does nothing.

## Develop

The `test` skill (`.claude/skills/test/SKILL.md`) gives every command to set
up, check, release and roll back.

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo test -- --ignored real_claude   # real Claude Code sessions
WORKER_SERVICE_URL=… WORKER_SERVICE_TOKEN=… cargo test --test worker_service
```

The tests run stand-ins for Claude Code; `real_claude` runs Claude Code
itself and checks it is the release the host is checked against. A reviewer runs
`tests/worker_service.rs` against beta before a release, as the test skill
(`.claude/skills/test/SKILL.md`) says.
