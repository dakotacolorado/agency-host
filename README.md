# worker-host

One static binary per machine. It sends heartbeats to `worker-service` and
keeps a socket open to it, makes and keeps workers' folders, runs their Claude
Code sessions, carries messages to them and what they say back.

Installing it is copying the file.

## Stand up a host

On a mac or a linux machine that is signed in to tailscale, with `gh` signed
in to read the releases, and Claude Code installed and signed in:

```sh
WORKER_HOST_SERVICE_URL=https://… WORKER_HOST_TOKEN=… scripts/install-host.sh
```

It installs tmux if it is missing, makes sure the tailnet can reach the
machine over ssh (and that nothing publishes it beyond the tailnet), downloads
the release for this machine and checks it against `SHA256SUMS`, proves it
with one heartbeat, and only then installs it as a service that runs as you,
survives logout and reboot, and restarts if it dies: a launchd daemon on a
mac, a lingering systemd user unit on linux. Workers are attached with `ssh`
over the tailnet and `tmux attach`; the host itself only calls out.

Run it again to take a newer release (`--release TAG` for a given one,
`--binary PATH` for a local build). `scripts/install-host.sh rollback` swaps
back to the binary installed before, and `remove` takes worker-host and its
config off the machine. The token lives only in the config file, mode 600.

A machine that ran agent-host keeps running it until it is removed with that
release's own `install-host.sh remove`: the two are separate services, with
separate configs, sockets and folders.

## Install by hand

Each release carries one binary per target, and `SHA256SUMS`:

| machine                  | file                                     |
| ------------------------ | ---------------------------------------- |
| linux, x86_64            | `worker-host-x86_64-unknown-linux-musl`  |
| linux, arm64             | `worker-host-aarch64-unknown-linux-musl` |
| mac, apple silicon       | `worker-host-aarch64-apple-darwin`       |
| mac, intel               | `worker-host-x86_64-apple-darwin`        |

```sh
gh release download --repo dakotacolorado/worker-host \
  --pattern worker-host-aarch64-apple-darwin --output ~/.local/bin/worker-host
chmod +x ~/.local/bin/worker-host
```

## Configure

A file, `~/.config/worker-host/config.toml` unless `--config` or
`WORKER_HOST_CONFIG` names another, and the environment, which overrides it:

```toml
service_url = "https://…"   # WORKER_HOST_SERVICE_URL
token = "…"                 # WORKER_HOST_TOKEN
host_id = "studio"          # WORKER_HOST_ID; defaults to the hostname
capacity = 2                # WORKER_HOST_CAPACITY
labels = ["macos", "repo:workplace-ui"]  # WORKER_HOST_LABELS, comma-separated
repositories = { workplace-ui = "/Users/me/src/workplace-ui" }
                            # WORKER_HOST_REPOSITORIES, as name=/path,...
workers_dir = "/Users/me/workers"  # WORKER_HOST_WORKERS_DIR; ~/workers is the default
tmux_socket = "worker-host" # WORKER_HOST_TMUX_SOCKET; this is the default
```

`capacity` is how many workers this host will run sessions for at once, and
`labels` what it can be matched to. Both are the owner's to say: a host with
no capacity still sends heartbeats, but the service puts no worker on it. A
label the service cannot take is refused with the pattern it must match, and
the host logs that refusal and waits rather than sending heartbeats without
it. `repositories` are the checkouts on this machine that a worker can be
given a worktree of, by the name a worker's `repositories` use; listing one is
not advertising it, which is a `repo:` label's job. `install-host.sh` takes
these and `workers_dir` from the same variables, and keeps the ones an earlier
install wrote.

A host lists every worker in `workers_dir` as its own, and every session on
its tmux server, so two hosts on one machine -- one sending heartbeats to beta
beside one sending heartbeats to prod -- each need a `workers_dir` and a
`tmux_socket` of their own.

Which service a host sends heartbeats to is only this, so the same binary tries
a change against beta before any machine is asked to take it:

```sh
WORKER_HOST_SERVICE_URL=https://beta… WORKER_HOST_TOKEN=… \
  WORKER_HOST_WORKERS_DIR=/tmp/beta-workers WORKER_HOST_TMUX_SOCKET=beta worker-host --heartbeats 3
```

`--heartbeats N` sends N and stops, with no socket.

## Run

`worker-host` runs until it is killed, and outlives the terminal that started
it. It sends a heartbeat every 30 seconds -- liveness, how loaded the machine
is, every worker's state, what they said, and how each command went -- and
keeps open the socket the heartbeat's reply names, with its token in the
`Authorization` header. A command pushed on the socket is taken at once, and
what changes -- a worker's state, a line of its transcript, a command's result
-- is reported on the socket within a second. The heartbeat is what is relied
on: the service sends a command on every heartbeat until a report of it
lands, so a socket that is down only makes the host slower to hear and be
heard. The socket is pinged every five minutes, since API Gateway closes one
idle for ten, and is opened again, backing off up to a minute, whenever it
closes. A heartbeat that fails is retried; a machine that sleeps simply sends
its next one late.

A load reading the host cannot take is left out, never sent as zero, and the
service keeps the last one it had.

Each heartbeat carries the version and the commit the binary was built from,
which `worker-host --version` also prints, with the contract it speaks:

```sh
$ worker-host --version
worker-host 0.1.0 (contract 1) 5812e39c0d…
```

A build from a tree with uncommitted changes says `dirty` instead of a commit,
and one built without git, or from source its enclosing repo does not track,
says `unknown`.

## Workers

A worker is created on a host by a `create`, and lives in a folder of its own,
`<workers_dir>/<name>/`:

```text
worker.json       who it is: id, name, role, repositories
credential        its own token for the service, mode 600
sessions.jsonl    each session the host started for it, and each it stopped
prompt.md         the opening message of its latest session
repos/<repo>/     a git worktree of this host's checkout, on branch worker/<name>
home/.claude/     its own Claude Code: settings, rules, history, transcripts
logs/<id>.log     everything each session's terminal showed, by session id
```

A create sent again is taken up where the first left off. A repository this
host has no checkout of, or a worktree that cannot be made -- origin cannot be
fetched, the branch is checked out elsewhere -- fails it with that reason. The
branch is taken from origin when it has been pushed there. git will not check
one branch out in two worktrees, so two workers never share a tree, and the
person's checkout is never the one a worker works in. `node_modules` is linked
from the checkout when it has one and left absent when it has not; the host
never installs into it. Worktrees are locked, so `git worktree prune` in the
checkout cannot take them by accident.

A `start` runs one Claude Code session for the worker, as the tmux session
`worker-<name>`, in its folder: a new conversation, or with `resume` its last
one carried on (`claude --continue` in the same folder). Its text is the first
thing the session is told. The host logs the session's terminal with
`tmux pipe-pane`. A start the host has already done -- its session is
running, or ran and has ended -- is done and not repeated.

Every worker has its own Claude Code state and the host user's Claude Code
login. HOME is the user's: on macOS the login is in the user's login Keychain,
which a process given another HOME does not search, so Claude Code would say
"Not logged in", and git, gh and ssh find the user's identity there too. Each
session is given `CLAUDE_CONFIG_DIR=<folder>/home/.claude` -- its settings,
history and transcripts -- and `CLAUDE_SECURESTORAGE_CONFIG_DIR`, which tells
Claude Code where the login is: empty for Claude Code's default (the
unsuffixed Keychain item on macOS, `~/.claude/.credentials.json` on linux), or
the host user's own `CLAUDE_CONFIG_DIR` if the host runs with one. The host
answers beforehand the questions a new Claude Code asks -- onboarding, the
trust of its folder, skipping permissions, Claude in Chrome -- and keeps its
standing rules, `src/conduct.md`, as the worker's `CLAUDE.md`, rewritten each
session so they follow the host's version: push its own branch and nothing
else, never force, merge or deploy, run the repository's own checks, ask when
stuck, and how to act on the service as itself.

Each session has `WORKER_SERVICE_URL`, the service this host sends heartbeats
to, `WORKER_ID`, and `WORKER_TOKEN`, its own credential, with which it acts on
the service as itself. The credential comes in the `create`; the first one
sent is kept. The session's shell reads it into the environment as Claude
Code starts, so it is never a process's argument, which anyone on the machine
can list; it is in no prompt and no log; and where a worker prints it, the
transcript the host sends says `[credential]` instead. The host itself keeps
sending heartbeats with its own token.

Every worker not retired is in each report: `working`, `waiting` once its
transcript shows its last turn ended and nothing said to it since, `blocked`
while Claude Code holds its terminal on a question (read from the foot of the
screen, since nothing is written until it is answered), `idle` with no
session, and `interrupted` when its last session ended without a stop -- a
crash, the machine going down -- which can be resumed. A host that restarts
picks up every session still running, and a worker whose session is gone
reads as interrupted.

The transcript is sent as it is written: what the worker was told, what it
said and what it ran, numbered across all its sessions, each line naming the
session it was written in, so a host that restarts sends the same lines again
under the same numbers and the service keeps each once.

A `message` is typed into the session once Claude Code is listening -- once it
has answered something in this session, since what is typed while it starts
is lost -- and reported done only when the transcript shows the worker was
told it. One not delivered within two minutes is reported failed with why. An
`interrupt` presses Escape, which stops the turn the worker is in without
ending the session; any text it carries is typed once the turn has stopped. A
`stop` ends the session and the worker is idle; its folder and branches are
left as they are. Stopping a worker with no session is done, not failed.

A `retire` stops the worker and moves its folder to
`<workers_dir>/.retired/<name>-<id>/`, where it is kept: its worktrees are
told where they went, and its branches renamed `worker/<name>-retired-<id>`,
so its name can be given to a new worker. What it pushed stays on origin.

A command the host has settled and is sent again -- the socket and the
heartbeat both carry it -- is answered as before, not done again. A kind the
host does not handle is reported failed, naming the kind.

To watch or talk to one directly on the machine:

```sh
tmux -L worker-host ls                 # or the host's tmux_socket
tmux -L worker-host attach -t worker-<name>
```

The machine needs `tmux` and `claude` on its PATH, with Claude Code signed in.

## Develop

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo test -- --ignored real_claude   # real Claude Code sessions, end to end
WORKER_SERVICE_URL=… WORKER_SERVICE_TOKEN=… cargo test --test worker_service
```

The tests need `tmux`, and use tmux servers of their own. The last runs the
binary against a deployed service, and without both variables says so and
passes.

CI runs those, builds every target, runs the linux binary through a whole
cycle against the beta service -- by heartbeat and by socket -- and on a merge
to main publishes the release.
