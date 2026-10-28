# worker-host

One static binary per machine. It sends heartbeats to `worker-service` and
keeps a socket open to it, makes workers' folders, runs their Claude Code
sessions in tmux, carries messages to them and what they say back.

## Install

On a mac or linux machine signed in to tailscale, with `gh` signed in and
Claude Code installed and signed in:

```sh
WORKER_HOST_SERVICE_URL=https://… WORKER_HOST_TOKEN=… scripts/install-host.sh
```

It installs tmux if missing, checks the tailnet can reach the machine over
ssh, downloads the release and checks it against `SHA256SUMS`, proves it with
one heartbeat, and installs it as a service (a launchd daemon on a mac, a
lingering systemd user unit on linux). Run it again for a newer release
(`--release TAG`, or `--binary PATH` for a local build); `rollback` swaps back
to the previous binary, `remove` takes it off the machine.

## Configure

`~/.config/worker-host/config.toml` (or `--config`, or `WORKER_HOST_CONFIG`);
the environment overrides it:

```toml
service_url = "https://…"          # WORKER_HOST_SERVICE_URL
token = "…"                        # WORKER_HOST_TOKEN
host_id = "studio"                 # WORKER_HOST_ID; defaults to the hostname
capacity = 2                       # WORKER_HOST_CAPACITY; unset, nothing is placed here
labels = ["macos"]                 # WORKER_HOST_LABELS, comma-separated
workers_dir = "/Users/me/workers"  # WORKER_HOST_WORKERS_DIR; default ~/workers
tmux_socket = "worker-host"        # WORKER_HOST_TMUX_SOCKET; this is the default
```

Two hosts on one machine (beta beside prod) each need their own `workers_dir`
and `tmux_socket`. `worker-host --heartbeats N` sends N heartbeats and stops,
with no socket. `worker-host --version` prints the version and commit.

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

The tests need `tmux` and use tmux servers of their own. CI also runs
`scripts/heartbeat-cycle.sh` against beta (create, start, stop, retire, by
heartbeat and by socket) and publishes the release on a merge to main.
