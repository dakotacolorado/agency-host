# worker-host

One static binary per machine. It sends heartbeats to `worker-service`, which
say only that it is here and what it offers, and keeps a socket open to it:
every command comes down the socket and every report goes up it. It makes
workers' folders, runs their Claude Code sessions in tmux, carries messages to
them and what they say back.

## Install

On a mac or linux machine with `tmux`, `gh` signed in, and Claude Code
installed and signed in:

```sh
scripts/install-host.sh https://… <token>
~/.local/bin/worker-host 2>> ~/worker-host.log &
```

The script puts the latest release in `~/.local/bin` and, if there is none,
writes the config file below. Workers' shared context comes with each
session's start (see Workers), so nothing else is installed; a run also takes
away the worker-skills clone at `<workers_dir>/.claude` and its cron sync that
earlier versions installed. Run it again for a newer release.

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
tmux_socket = "worker-host"        # this is the default
```

Two hosts on one machine (beta beside prod) each need their own `workers_dir`
and `tmux_socket`. `worker-host --version` prints the release it was built as
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
credentials.env   the latest session's tokens, readable by the host's user alone
home/.claude/     its own Claude Code (CLAUDE_CONFIG_DIR): settings, CLAUDE.md, transcripts
logs/<id>.log     everything each session's terminal showed
```

A `start` may carry a context: files written into `home/.claude/` before
Claude Code starts, `skills/...` as files (scripts executable) and a
`CLAUDE.md` appended to the host's own rules, in place of the last context's
(listed in `home/.claude/.context.json`); a start without one keeps the last.
With it may come `documents`, written after it the same way, each `CLAUDE.md`
after the one before. It may also carry variables to set in the session, such
as the services' addresses, and `secrets`, their tokens: those are written to
`credentials.env`, mode 600, and read into the session's environment as it
starts, so no token is on tmux's or any other command line. The host does not
know where any of it comes from.

The worker clones whatever code it needs itself. A `start` runs Claude Code
as tmux session `worker-<name>` in that folder, a new conversation or, with
`resume`, `claude --continue`. Each session shares the host user's Claude
login: HOME stays the user's, and `CLAUDE_SECURESTORAGE_CONFIG_DIR` points at
the user's own store (see `src/claude.rs`).
A worker's settings deny the `AskUserQuestion`, `EnterPlanMode` and
`ExitPlanMode` tools: their dialogs are seen only at the terminal, and a
message typed into one picks its first option, so a worker asks in words and
ends its turn instead. Behind that, a message is not typed while the terminal
shows any dialog: it waits for the dialog to close, and if it has not within
the delivery's two minutes, the service is told it was never typed and why.

States: `working`, `waiting` (its last turn ended, nothing said since),
`idle` (no session), `interrupted` (its session ended without a stop; can be
resumed). A `message` is typed once Claude Code is listening and reported
done once the transcript shows it; `interrupt` presses Escape; `stop` ends
the session; `retire` stops it and moves the folder to
`<workers_dir>/.retired/<name>-<id>/`. A host that restarts picks up every
live session. Every command is idempotent: one sent again is answered, not
redone.

```sh
tmux -L worker-host attach -t worker-<name>
```

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

The tests need `tmux` and use tmux servers of their own. A reviewer runs
`tests/worker_service.rs` against beta before a release, as the test skill
(`.claude/skills/test/SKILL.md`) says.
