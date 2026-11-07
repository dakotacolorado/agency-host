---
name: test
description: How to run worker-host's checks and its beta test, and which of its pipeline's jobs check, release and roll it back. Use before submitting a change to worker-host, and when reviewing one.
---

# Testing worker-host

## What beta and prod mean here

worker-host is not deployed: it is a binary that each machine installs with
`scripts/install-host.sh`, which takes the GitHub release marked Latest.

- **Beta** is `tests/worker_service.rs`: it builds this binary, runs it as a
  host of its own against worker-service's beta stage, and drives a worker
  through create, start, message, stop and retire over the socket. There is no
  standing beta host: beta's `/fleet/hosts` lists only hosts long `lost`.
- **Prod** is publishing a release. Hosts take it only when their owner runs
  `scripts/install-host.sh` again and restarts the host; nothing pushes it to
  them. Installing on a machine is the person's step.

## The pipeline

`pipeline.toml` is worker-host's release, step by step, and `jobs.toml` holds
the command each job runs, its `run` (approval-service's `docs/PIPELINE.md`),
each in the image `Containerfile` builds on base-rust (Rust, zig and
cargo-zigbuild from base-rust; the musl and macOS targets, `gh`, the AWS CLI
and `file` added), from a clean checkout. This skill names the jobs; their
commands are only there, so read them there.

| Step | Jobs |
|---|---|
| `build` | `build`: the release's own build of all four binaries, checked as it checks them, publishing nothing |
| `unit-test` | `test`, `clippy`, `fmt` |
| `deploy-beta` | `beta-test`: the beta test against WorkerService-beta, then `deployed: <commit>` |
| `integration-test-beta`, `rollback-beta` | skipped: the beta test is `deploy-beta`, and no host stays on beta after it |
| `deploy-prod` | `release`: publishes the next release as Latest, then `deployed: <commit> <tag>` |
| `integration-test-prod` | `check-prod`: read-only checks of the Latest release |
| `rollback-prod` | `rollback`, at the good release's commit: makes that release Latest again |

**Releases and rollbacks are approval-service's**, run as the `release` and
`rollback` jobs: it runs every step, records the releases from the
`deployed:` line, and a rollback is the `rollback` job submitted at the last
good release's commit (`approval releases dakotacolorado/worker-host`). No
author or agent runs `release` or `rollback`. A host that installed a bad release
keeps it until its owner runs `scripts/install-host.sh` again.

## Setup

`gh` signed in. A Rust toolchain with rustfmt and clippy, and a C
compiler (ring, under rustls, has C). On a machine without them, `setup.sh`
puts rustup and zig (as `cc`, `ar` and `zig`) under the folder you name (a
worker names `tools/` in its own folder) and prints the line that puts them on
PATH:

```sh
eval "$(.claude/skills/test/setup.sh /home/<you>/workers/<worker>/tools)"
```

Run it again in each new shell; it skips what is there. AWS credentials for
the account (`aws sts get-caller-identity`), region `us-east-1`, for the beta
test. A job needs none of this on the machine submitting it.

## The checks: `build`, `unit-test` and the beta test

An author runs `test`, `clippy` and `fmt` at the PR's head before submitting
it, and a reviewer at the commit reviewed; `build` and `beta-test` too, for a
change to the build or to how the host speaks to worker-service. Each must
exit 0. Two ways, either is enough:

- **As jobs**, at a pushed commit, with job-service's client (`skill/bin/jobs`
  in job-service, which needs `JS_URL`/`JS_TOKEN`):

  ```sh
  jobs submit dakotacolorado/worker-host <commit> <job>    # prints the job, with its id
  jobs log <id> -f    # follows its log, then exits with the job's exit code
  ```

  A failed job's log ends with what failed; `jobs show <id>` gives its exit
  code and reason.

- **By hand**, in your clone with the setup above: run each job's `run` from
  `jobs.toml`, from the repo root, with the session's prod `*_URL`/`*_TOKEN`
  unset:

  ```sh
  env -u DS_URL -u DS_TOKEN -u WT_URL -u WT_TOKEN -u WS_URL -u WS_TOKEN -u AS_URL -u AS_TOKEN -u JS_URL -u JS_TOKEN <the job's run>
  ```

Run each on its own and read its exit status (`echo $?`). Never pipe one into
`tail` or `head`: the pipe reports their status, so a failure reads as a pass.

The tests run stand-ins for Claude Code. Without `WORKER_SERVICE_URL`,
`tests/worker_service.rs` prints `skipped:` and passes, so `test` proves
nothing about beta: `beta-test` does.

`scripts/rollback` takes `DRY_RUN=1` to do all but edit, which an author may
run: in a checkout of an older release's commit, it says which releases it
would edit.

After `pipeline.toml` or `jobs.toml` changes, check them:

```sh
"$CLAUDE_CONFIG_DIR/skills/approval/bin/approval" pipeline . dakotacolorado/worker-host; echo "exit $?"
```

### Claude Code itself (by hand, no job)

Claude Code itself, headless over stream-json, costs real sessions, and is
checked against the release in `claude::CHECKED`: run it for a change to how
the host speaks to Claude Code, and when Claude Code on the host is not that
release. `CLAUDE_CONFIG_DIR` is unset so the sessions use the user's own login:

```sh
env -u CLAUDE_CONFIG_DIR -u DS_URL -u DS_TOKEN -u WT_URL -u WT_TOKEN -u WS_URL -u WS_TOKEN -u AS_URL -u AS_TOKEN cargo test --locked -- --ignored real_claude
```

A pass: `2 passed`. `real_claude` fails first on a Claude Code that is not
`CHECKED`; once it passes on another, that release is the new `CHECKED`.

## Beta test: `beta-test` (beta only)

Against beta only, never prod, so anyone may run it. It runs
`tests/worker_service.rs` with `WORKER_SERVICE_URL` from stack
`WorkerService-beta`'s `ApiUrl` and `WORKER_SERVICE_TOKEN` from secret
`worker-service/beta/token` (not `api-token`), never printed, and fails if the
test says `skipped:`. A pass: `create`, `start`, `message`, `stop` and
`retire` each `done`, then `test result: ok. 1 passed`, then
`deployed: <commit>`, exit 0. Any failure is a failure. The test's host sets
no capacity, so the service places no other worker on it: when it set
`capacity = 1`, a document-service judge made on beta without a host once
landed there and the test's own create got
`409 test-<pid> is running all 1 sessions it will.`

## Prod read-only checks: `check-prod`

It checks what hosts install now, whichever commit it runs at: the Latest
release's tag, the commit it was built from and its `5` assets (four binaries
and `SHA256SUMS`); each binary matches `SHA256SUMS`, and the linux one runs
and says `worker-host <the tag>` (`worker-host 0.1.0` for releases up to
v0.1.0-47, built before the build was given its tag).

Which hosts are connected to prod (`WS_URL`/`WS_TOKEN` are the session's), by
hand:

```sh
printf 'Authorization: Bearer %s\n' "$WS_TOKEN" | curl -sf -H @- "$WS_URL/fleet/hosts"
```

A pass: 200 and each host the person runs `"status":"connected"`. A host's
`version` is the release tag it runs (e.g. `v0.1.0-48`); `0.1.0` is a release
up to v0.1.0-47, or a build that is not a release.
