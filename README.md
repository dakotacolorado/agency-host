# agent-host

One static binary per machine. It beats to `agent-service`, starts and stops
agents, carries messages to them and their output back.

Installing it is copying the file.

## Stand up a host

On a mac or a linux machine that is signed in to tailscale, with `gh` signed
in to read the releases:

```sh
AGENT_HOST_SERVICE_URL=https://… AGENT_HOST_TOKEN=… scripts/install-host.sh
```

It installs tmux if it is missing, makes sure the tailnet can reach the
machine over ssh (and that nothing publishes it beyond the tailnet), downloads
the release for this machine and checks it against `SHA256SUMS`, proves it
with one beat, and only then installs it as a service that runs as you,
survives logout and reboot, and restarts if it dies: a launchd daemon on a
mac, a lingering systemd user unit on linux. Workers are attached with
`ssh` over the tailnet and `tmux attach`; the host itself only calls out.

Run it again to take a newer release (`--release TAG` for a given one,
`--binary PATH` for a local build). `scripts/install-host.sh rollback` swaps
back to the binary installed before, and `remove` takes agent-host and its
config off the machine. The token lives only in the config file, mode 600.

## Install by hand

Each release carries one binary per target, and `SHA256SUMS`:

| machine                  | file                                   |
| ------------------------ | -------------------------------------- |
| linux, x86_64            | `agent-host-x86_64-unknown-linux-musl`  |
| linux, arm64             | `agent-host-aarch64-unknown-linux-musl` |
| mac, apple silicon       | `agent-host-aarch64-apple-darwin`       |
| mac, intel               | `agent-host-x86_64-apple-darwin`        |

```sh
gh release download --repo dakotacolorado/agent-host \
  --pattern agent-host-aarch64-apple-darwin --output ~/.local/bin/agent-host
chmod +x ~/.local/bin/agent-host
```

## Configure

A file, `~/.config/agent-host/config.toml` unless `--config` or
`AGENT_HOST_CONFIG` names another, and the environment, which overrides it:

```toml
service_url = "https://…"   # AGENT_HOST_SERVICE_URL
token = "…"                 # AGENT_HOST_TOKEN
host_id = "studio"          # AGENT_HOST_ID; defaults to the hostname
capacity = 2                # AGENT_HOST_CAPACITY
labels = ["macos", "repo:agent-service"]  # AGENT_HOST_LABELS, comma-separated
repositories = { agent-service = "/Users/me/src/agent-service" }
                            # AGENT_HOST_REPOSITORIES, as name=/path,...
```

`capacity` is how many agents this host will run at once, and `labels` what
work it can be matched to. Both are the owner's to say: a host with no
capacity still beats, but the service places nothing on it. A label the
service cannot match on is refused with the pattern it must match, and the
host logs that refusal and waits rather than beating without it.
`repositories` are the checkouts on this machine that agents can be given a
worktree of, by the name a placement uses; listing one is not advertising it,
which is a `repo:` label's job. `install-host.sh` takes all three from the
same variables, and keeps the ones an earlier install wrote.

Which service a host beats to is only this, so the same binary tries a change
against beta before any machine is asked to take it:

```sh
AGENT_HOST_SERVICE_URL=https://beta… AGENT_HOST_TOKEN=… agent-host --beats 3
```

## Run

`agent-host` runs until it is killed, and outlives the terminal that started
it. A beat that fails is retried; a machine that sleeps simply beats late.

Each beat also carries how loaded the machine is -- cpu, memory, and the disk
agents work on -- and the tmux sessions a person has open on it, which are
not agents: agents run on the host's own tmux server and are listed apart. A
reading the host cannot take is left out, never sent as zero, and the service
keeps the last one it had.

Each beat carries the version and the commit the binary was built from, which
`agent-host --version` also prints, so a machine can be asked directly:

```sh
$ agent-host --version
agent-host 0.1.0 5812e39c0d…
```

A build from a tree with uncommitted changes says `dirty` instead of a commit,
and one built without git, or from source its enclosing repo does not track,
says `unknown`.

## Agents

Each agent is a Claude Code session in tmux, on the host's own socket, so it
keeps a terminal and can be spoken to while it works. A `start` command runs
one, named by the id the service gave it, in a folder of its own under
`$XDG_DATA_HOME/agent-host/agents` (or `~/.local/share/agent-host/agents`). A start the host has already done --
its session is running, or it has run and left a transcript -- is reported
done and not repeated, so a host that restarts before reporting does not run
the agent twice. Every running agent is listed on each beat.

An agent placed for a repository starts in a git worktree of this host's
checkout of it, at `agents/<id>/<repository>`, on the branch the placement
named -- taken from origin when it has been pushed there -- or else on
`agent/<id>`. git will not check one branch out in two worktrees, so two
agents on one host never share a tree, and the person's checkout is never the
one an agent works in. `node_modules` is linked from the checkout when it has
one and left absent when it has not; the host never installs into it. If the
worktree cannot be made -- no checkout by that name, origin cannot be
fetched, the branch is already checked out elsewhere -- the start fails with
that reason and no agent runs.

Every agent is started with the standing rules in `src/conduct.md` -- push
its own branch and nothing else, never force, merge or deploy, run the
repository's own checks, ask when stuck -- and then its brief, exactly as placed. The prompt is kept at
`agents/<id>/prompt.md`.

A workspace is kept when its agent ends, for inspection, and its worktree is
locked so that `git worktree prune` in the checkout cannot take it by
accident. Removing one is deliberate:
`git -C <checkout> worktree remove --force --force <workspace>`.

A message is typed into the session at once; Claude Code takes it at its next
step, or, if the agent has stopped to ask, as the answer, and the same run goes
on. An `interrupt` presses Escape in the session, which stops the turn the
agent is in, mid-command if need be, without ending the session; any text it
carries is typed in once the turn has stopped. The host reads the session's
transcript as it is written and sends it up on each beat, in that agent's
report: what it was told, what it said and what it ran, numbered from the
transcript's start, so a host that restarts sends the same lines again under
the same numbers and the service keeps each once.

A command kind the host does not handle is reported failed, naming the kind.

To watch or talk to one directly on the machine:

```sh
tmux -L agent-host ls
tmux -L agent-host attach -t <agent>
```

The machine needs `tmux` and `claude` on its PATH, with Claude Code signed in.

## Develop

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
cargo test -- --ignored real_claude   # real Claude Code sessions, end to end
```

The tests need `tmux`.

CI runs those, builds every target, runs the linux binary through a whole beat
cycle against the beta service, and on a merge to main publishes the release.
