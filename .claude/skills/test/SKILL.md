---
name: test
description: How to validate, release and check worker-host. Use before submitting a change to worker-host, and when reviewing, releasing or rolling one back.
---

# Testing worker-host

An author runs setup and the checks, and may run the beta integration test and
the read-only checks. **Releasing and rolling back are for a reviewer only**: a
worker started with the `approval` document and an objective to review. An
author never runs them.

Run every command from the repo root, each on its own, and read its exit
status (`echo $?`). Never pipe one into `tail` or `head`: the pipe reports
their status, so a failure reads as a pass.

## What beta and prod mean here

worker-host is not deployed: it is a binary that each machine installs with
`scripts/install-host.sh`, which takes the GitHub release marked Latest.

- **Beta** is `tests/worker_service.rs`: it builds this binary, runs it as a
  host of its own against worker-service's beta stage, and drives a worker
  through create, start, message, stop and retire over the socket. There is no
  standing beta host: beta's `/fleet/hosts` lists only hosts long `lost`.
- **Prod** is publishing a release. Hosts take it only when their owner runs
  `scripts/install-host.sh` again and restarts the host; nothing pushes it to
  them. Installing on a machine is the person's step, not the reviewer's.

GitHub Actions is off, so nothing builds, tests against beta or releases on
its own: the pipeline's jobs below do, or the by-hand commands after them.

## The pipeline

Every step of a release is a job: `jobs.toml` defines them, `pipeline.toml`
says which step runs which (approval-service's `docs/PIPELINE.md`), and each
runs in the image `Containerfile` builds on base-rust (Rust, zig and
cargo-zigbuild from base-rust; the musl and macOS targets, `gh`, the AWS CLI
and `file` added), from a clean checkout:

| Step | Job | Runs |
|---|---|---|
| `build` | `build` | `DRY_RUN=1 scripts/release`: the release's four builds and checks, publishing nothing |
| `unit-test` | `test`, `clippy`, `fmt` | the checks below |
| `deploy-beta` | `beta-test` | `bin/beta-test`: the beta test against WorkerService-beta, then `deployed: <commit>` |
| `integration-test-beta` | skipped | the beta test is `deploy-beta`; no host stays on beta after it |
| `rollback-beta` | skipped | nothing stays on beta |
| `deploy-prod` | `release` | `scripts/release`: publishes the next release as Latest, then `deployed: <commit> <tag>` (reviewer only) |
| `integration-test-prod` | `check-prod` | `bin/check-prod`: the prod read-only checks of the Latest release |
| `rollback-prod` | `rollback` | `scripts/rollback` at the good release's commit (reviewer only) |

`bin/` is `.claude/skills/test/bin/`. An author may run `build`, `test`,
`clippy`, `fmt`, `beta-test` and `check-prod`; never `release` or `rollback`.
Run one as approval-service does, at a pushed commit:

```sh
jobs submit dakotacolorado/worker-host <commit> test    # job-service's skill/bin/jobs
```

`check-prod` checks Latest, whichever commit you run it at. Both
`scripts/release` and `scripts/rollback` take `DRY_RUN=1` to do all but
publish or edit, which an author may run by hand: `scripts/rollback`, run in a
checkout of an older release's commit, says which releases it would edit.

After `pipeline.toml` or `jobs.toml` changes, check them:

```sh
"$CLAUDE_CONFIG_DIR/skills/approval/bin/approval" pipeline . dakotacolorado/worker-host; echo "exit $?"
```

The sections below are the same steps by hand, for when the jobs cannot run.

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
test.

## Checks: `build` and `unit-test`

The session's `*_URL`/`*_TOKEN` are prod's, so every check runs with them
unset. Each must exit 0.

```sh
env -u DS_URL -u DS_TOKEN -u WT_URL -u WT_TOKEN -u WS_URL -u WS_TOKEN -u AS_URL -u AS_TOKEN cargo fmt --check
env -u DS_URL -u DS_TOKEN -u WT_URL -u WT_TOKEN -u WS_URL -u WS_TOKEN -u AS_URL -u AS_TOKEN cargo clippy --all-targets --locked -- -D warnings
env -u DS_URL -u DS_TOKEN -u WT_URL -u WT_TOKEN -u WS_URL -u WS_TOKEN -u AS_URL -u AS_TOKEN cargo test --locked
```

The tests run stand-ins for Claude Code. Without `WORKER_SERVICE_URL`,
`tests/worker_service.rs` prints `skipped:` and passes, so `cargo test` proves
nothing about beta.

Claude Code itself, headless over stream-json, costs real sessions, and is
checked against the release in `claude::CHECKED`: run it for a change to how
the host speaks to Claude Code, and when Claude Code on the host is not that
release. `CLAUDE_CONFIG_DIR` is unset so the sessions use the user's own login:

```sh
env -u CLAUDE_CONFIG_DIR -u DS_URL -u DS_TOKEN -u WT_URL -u WT_TOKEN -u WS_URL -u WS_TOKEN -u AS_URL -u AS_TOKEN cargo test --locked -- --ignored real_claude
```

A pass: `2 passed`. `real_claude` fails first on a Claude Code that is not
`CHECKED`; once it passes on another, that release is the new `CHECKED`.

## Beta: the integration test, `deploy-beta`

Against beta only, never prod. `bin/beta-test` runs it as below, failing if
it says `skipped:`. Its URL is stack `WorkerService-beta`'s
`ApiUrl`; its token is secret `worker-service/beta/token` (not `api-token`),
never printed.

```sh
WORKER_SERVICE_URL=$(aws cloudformation describe-stacks --stack-name WorkerService-beta --query "Stacks[0].Outputs[?OutputKey=='ApiUrl'].OutputValue" --output text) WORKER_SERVICE_TOKEN=$(aws secretsmanager get-secret-value --secret-id worker-service/beta/token --query SecretString --output text) env -u DS_URL -u DS_TOKEN -u WT_URL -u WT_TOKEN -u WS_URL -u WS_TOKEN -u AS_URL -u AS_TOKEN cargo test --locked --test worker_service -- --nocapture
```

A pass: `create`, `start`, `message`, `stop` and `retire` each `done`, then
`test result: ok. 1 passed`, exit 0. Any failure is a failure. The test's host
sets no capacity, so the service places no other worker on it: when it set
`capacity = 1`, a document-service judge made on beta without a host once
landed there and the test's own create got
`409 test-<pid> is running all 1 sessions it will.`

## Prod read-only checks: `integration-test-prod`

What hosts install now, and that it runs here (`bin/check-prod` does this for
all four binaries):

```sh
gh release view --repo dakotacolorado/worker-host --json tagName,targetCommitish,assets --jq '.tagName, .targetCommitish, (.assets | length)'
dir=$(mktemp -d) && gh release download --repo dakotacolorado/worker-host --pattern 'worker-host-x86_64-unknown-linux-musl' --pattern SHA256SUMS --dir "$dir" && (cd "$dir" && sha256sum --check --ignore-missing SHA256SUMS) && chmod +x "$dir/worker-host-x86_64-unknown-linux-musl" && "$dir/worker-host-x86_64-unknown-linux-musl" --version
```

A pass: the Latest tag, the commit it was built from, `5` assets (four
binaries and `SHA256SUMS`); then `worker-host-x86_64-unknown-linux-musl: OK`
and `worker-host <the tag>` (`worker-host 0.1.0` for releases up to v0.1.0-47,
built before the build was given its tag).

Which hosts are connected to prod (`WS_URL`/`WS_TOKEN` are the session's):

```sh
printf 'Authorization: Bearer %s\n' "$WS_TOKEN" | curl -sf -H @- "$WS_URL/fleet/hosts"
```

A pass: 200 and each host the person runs `"status":"connected"`. A host's
`version` is the release tag it runs (e.g. `v0.1.0-48`); `0.1.0` is a release
up to v0.1.0-47, or a build that is not a release.

## Release to prod: `deploy-prod` (reviewer only)

At the merged commit on `main`, clean tree, after the checks and the beta test
pass there. `scripts/release` does all of this but recording the releases,
which approval-service does from its `deployed:` line; by hand, on a machine
with the setup above and `file`, run it, or the steps below. The tag is numbered one past the last release, and the build is
given it in `WORKER_HOST_RELEASE`, so the binary reports it:

```sh
last=$(gh release list --repo dakotacolorado/worker-host --limit 1 --json tagName --jq '.[0].tagName') && tag="v$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)-$((${last##*-} + 1))" && echo "$tag"
```

Build every target with zig (the macOS ones too), in the same shell:

```sh
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl aarch64-apple-darwin x86_64-apple-darwin
cargo install --locked cargo-zigbuild@0.24.0
for t in x86_64-unknown-linux-musl aarch64-unknown-linux-musl aarch64-apple-darwin x86_64-apple-darwin; do WORKER_HOST_RELEASE="$tag" cargo zigbuild --release --locked --target $t || break; done
```

Each must end `Finished`; the macOS ones warn that `xcrun` is missing, which
is harmless. zig may leave `.intentionally-empty-file.o` in the repo root:
delete it.

The linux binary must need nothing on the machine and print `worker-host $tag`:

```sh
file target/x86_64-unknown-linux-musl/release/worker-host | grep 'statically linked'
target/x86_64-unknown-linux-musl/release/worker-host --version
```

Publish as `$tag`:

```sh
rm -rf target/dist && mkdir target/dist && for t in x86_64-unknown-linux-musl aarch64-unknown-linux-musl aarch64-apple-darwin x86_64-apple-darwin; do cp target/$t/release/worker-host target/dist/worker-host-$t; done && chmod +x target/dist/* && (cd target/dist && sha256sum worker-host-* > SHA256SUMS)
gh release create "$tag" target/dist/* --repo dakotacolorado/worker-host --target "$(git rev-parse HEAD)" --title "worker-host $tag" --notes "Built from $(git rev-parse HEAD) by hand: WORKER_HOST_RELEASE=$tag cargo zigbuild --release --locked for each target. Proved against WorkerService-beta with tests/worker_service.rs. Install with scripts/install-host.sh; see the README."
```

Then run the prod read-only checks: the tag is `$tag` and the commit is the
one you built. Record both releases (`POST /releases`) at that commit's full
40-character SHA (`git rev-parse HEAD`), never the tag, which approval-service
refuses; name the tag in the prod release's `checked`. Tell the person a release is out: their hosts take it when they run
`scripts/install-host.sh` and restart the host.

## Roll back: `rollback-prod` (reviewer only)

`scripts/rollback`, run in a checkout of the good release's commit, makes the
newest release at that commit Latest and marks each one numbered after it a
prerelease, as below. By hand, find the last good release:

```sh
approval releases dakotacolorado/worker-host
gh release list --repo dakotacolorado/worker-host --limit 5
```

With none recorded by approval-service, it is the release before the bad one.
Make it Latest again and take the bad one out of reach of `install-host.sh`:

```sh
gh release edit <good-tag> --repo dakotacolorado/worker-host --latest
gh release edit <bad-tag> --repo dakotacolorado/worker-host --prerelease
```

The prod read-only checks must then show `<good-tag>`. A host that already
installed the bad one keeps it until its owner runs `scripts/install-host.sh`
again: tell the person which hosts to reinstall.
