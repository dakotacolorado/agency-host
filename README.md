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
writes the config file below. Run it again for a newer release.

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
and `tmux_socket`. `worker-host --version` prints the version.

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
home/.claude/     its own Claude Code (CLAUDE_CONFIG_DIR): settings, CLAUDE.md, transcripts
logs/<id>.log     everything each session's terminal showed
```

The worker clones whatever code it needs itself. A `start` runs Claude Code
as tmux session `worker-<name>` in that folder, a new conversation or, with
`resume`, `claude --continue`. Each session shares the host user's Claude
login: HOME stays the user's, and `CLAUDE_SECURESTORAGE_CONFIG_DIR` points at
the user's own store (see `src/claude.rs`).

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

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo test -- --ignored real_claude   # real Claude Code sessions
WORKER_SERVICE_URL=… WORKER_SERVICE_TOKEN=… cargo test --test worker_service
```

The tests need `tmux` and use tmux servers of their own. CI runs
`tests/worker_service.rs` against beta and publishes a release on a merge to
main only when that test ran against beta and passed.
