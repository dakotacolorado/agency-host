---
name: test
description: How to run agent-host's checks and its beta test, and which of its pipeline's jobs check, release and roll it back. Use before submitting a change to agent-host, and when reviewing one.
---

# Testing agent-host

## What beta and prod mean here

agent-host is not deployed: it is a binary that each machine installs with
`scripts/install-host.sh`, which takes the GitHub release marked Latest.

- **Beta** is `tests/agent_service.rs`: it builds this binary, runs it as a
  host of its own against the agent-service that `AGENT_SERVICE_URL` names
  (a stage that is not prod, or one run on this machine for the test), and
  drives an agent through create, start, message, stop and retire over the
  socket. No host stays on it after the test.
- **Prod** is publishing a release. Hosts take it only when their owner runs
  `scripts/install-host.sh` again and restarts the host; nothing pushes it to
  them. Installing on a machine is the person's step.

## The pipeline

agent-host's release is the routine `dakotacolorado/release-routines` at
`dakotacolorado/agent-host/`: its `pipeline.toml` is the release, step by
step, and its `jobs.toml` holds the command each job runs, its `run`
(approval-service's `docs/PIPELINE.md`), changed only through the release
service. Each job runs with this repo checked out as its working directory,
in the image `Containerfile` builds on base-rust (Rust, zig and
cargo-zigbuild from base-rust; the musl and macOS targets, `gh` and `file`
added), from a clean checkout. This skill names the jobs; their commands are only in
the routine, so read them there.

| Step | Jobs |
|---|---|
| `build` | `build`: the release's own build of all four binaries, checked as it checks them, publishing nothing |
| `unit-test` | `test`, `clippy`, `fmt` |
| `deploy-beta` | `beta-test`: the beta test against `AGENT_SERVICE_URL`, then `deployed: <commit>` |
| `integration-test-beta`, `rollback-beta` | skipped: the beta test is `deploy-beta`, and no host stays on beta after it |
| `deploy-prod` | `release`: publishes the next release as Latest, then `deployed: <commit> <tag>` |
| `integration-test-prod` | `check-prod`: read-only checks of the Latest release |
| `rollback-prod` | `rollback`, at the good release's commit: makes that release Latest again |

**Releases and rollbacks are approval-service's**, run as the `release` and
`rollback` jobs: it runs every step, records the releases from the
`deployed:` line, and a rollback is the `rollback` job submitted at the last
good release's commit (`approval releases dakotacolorado/agent-host`). No
author or agent runs `release` or `rollback`. A host that installed a bad release
keeps it until its owner runs `scripts/install-host.sh` again.

## Setup

`gh` signed in. A Rust toolchain with rustfmt and clippy, and a C
compiler (ring, under rustls, has C). On a machine without them, `setup.sh`
puts rustup and zig (as `cc`, `ar` and `zig`) under the folder you name (an
agent names `tools/` in its own folder) and prints the line that puts them on
PATH:

```sh
eval "$(.claude/skills/test/setup.sh /home/<you>/agents/<agent>/tools)"
```

Run it again in each new shell; it skips what is there. For the beta test,
`AGENT_SERVICE_URL` and `AGENT_SERVICE_TOKEN`: an agent-service that is not
prod and its `AUTH_TOKEN`. A job needs none of this on the machine submitting it.

## The checks: `build`, `unit-test` and the beta test

An author runs `test`, `clippy` and `fmt` at the PR's head before submitting
it, and a reviewer at the commit reviewed; `build` and `beta-test` too, for a
change to the build or to how the host speaks to agent-service. Each must
exit 0. Two ways, either is enough:

- **As jobs**, at a pushed commit, with job-service's client (`skill/bin/jobs`
  in job-service, which needs `JS_URL`/`JS_TOKEN`):

  ```sh
  jobs submit --routine dakotacolorado/release-routines "$(git ls-remote https://github.com/dakotacolorado/release-routines.git refs/heads/main | cut -f1)" dakotacolorado/agent-host dakotacolorado/agent-host <commit> <job>    # prints the job, with its id
  jobs log <id> -f    # follows its log, then exits with the job's exit code
  ```

  A failed job's log ends with what failed; `jobs show <id>` gives its exit
  code and reason.

- **By hand**, in your clone with the setup above: run each job's `run` from
  the routine's `jobs.toml` (`dakotacolorado/agent-host/jobs.toml` in a clone
  of release-routines), from this repo's root, with the session's prod
  `*_URL`/`*_TOKEN` unset:

  ```sh
  env -u DS_URL -u DS_TOKEN -u WT_URL -u WT_TOKEN -u WS_URL -u WS_TOKEN -u AS_URL -u AS_TOKEN -u JS_URL -u JS_TOKEN -u AG_URL -u AG_TOKEN <the job's run>
  ```

Run each on its own and read its exit status (`echo $?`). Never pipe one into
`tail` or `head`: the pipe reports their status, so a failure reads as a pass.

The tests run stand-ins for Claude Code. Without `AGENT_SERVICE_URL`,
`tests/agent_service.rs` prints `skipped:` and passes, so `test` proves
nothing about beta: `beta-test` does.

`scripts/rollback` takes `DRY_RUN=1` to do all but edit, which an author may
run: in a checkout of an older release's commit, it says which releases it
would edit.

After the routine's `pipeline.toml` or `jobs.toml` changes, check them:

```sh
"$CLAUDE_CONFIG_DIR/skills/approval/bin/approval" pipeline <release-routines clone>/dakotacolorado/agent-host dakotacolorado/agent-host; echo "exit $?"
```

### Claude Code itself (by hand, no job)

Claude Code itself, headless over stream-json, costs real sessions, and is
checked against the release in `claude::CHECKED`: run it for a change to how
the host speaks to Claude Code, and when Claude Code on the host is not that
release. `CLAUDE_CONFIG_DIR` is unset so the sessions use the user's own login:

```sh
env -u CLAUDE_CONFIG_DIR -u DS_URL -u DS_TOKEN -u WT_URL -u WT_TOKEN -u WS_URL -u WS_TOKEN -u AS_URL -u AS_TOKEN -u JS_URL -u JS_TOKEN -u AG_URL -u AG_TOKEN cargo test --locked -- --ignored real_claude
```

A pass: `2 passed`. `real_claude` fails first on a Claude Code that is not
`CHECKED`; once it passes on another, that release is the new `CHECKED`.

## Beta test: `beta-test` (beta only)

Against beta only, never prod, so anyone may run it. It runs
`tests/agent_service.rs` against the agent-service at `AGENT_SERVICE_URL`
with `AGENT_SERVICE_TOKEN`, never printed, and fails without both or if the
test says `skipped:`. One run on this machine for it will do:

```sh
AUTH_TOKEN=t ADMIN_TOKEN=a node src/cli.ts --port <free port> --db <scratch>.db    # in agent-service
AGENT_SERVICE_URL=http://127.0.0.1:<port> AGENT_SERVICE_TOKEN=t .claude/skills/test/bin/beta-test
```
 A pass: `create`, `start`, `message`, `stop` and
`retire` each `done`, then `test result: ok. 1 passed`, then
`deployed: <commit>`, exit 0. Any failure is a failure. The test's host sets
no capacity, so the service places no other agent on it: when it set
`capacity = 1`, a document-service judge made on beta without a host once
landed there and the test's own create got
`409 test-<pid> is running all 1 sessions it will.`

## Prod read-only checks: `check-prod`

It checks what hosts install now, whichever commit it runs at: the Latest
release's tag, the commit it was built from and its `5` assets (four binaries
and `SHA256SUMS`); each binary matches `SHA256SUMS`, and the linux one runs
and says `agent-host <the tag>`.

Which hosts are connected to prod (`AG_URL`/`AG_TOKEN` are the session's), by
hand:

```sh
printf 'Authorization: Bearer %s\n' "$AG_TOKEN" | curl -sf -H @- "$AG_URL/hosts"
```

A pass: 200 and each host the person runs `"status":"connected"`. A host's
`version` is the release tag it runs (e.g. `v0.1.0-1`); `0.1.0` is a build
that is not a release.
