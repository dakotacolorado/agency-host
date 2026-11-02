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
its own; the commands below do it by hand.

## Setup

`tmux` and `gh` signed in. A Rust toolchain with rustfmt and clippy, and a C
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

## Checks

The session's `*_URL`/`*_TOKEN` are prod's, so every check runs with them
unset. Each must exit 0.

```sh
env -u DS_URL -u DS_TOKEN -u WT_URL -u WT_TOKEN -u WS_URL -u WS_TOKEN -u AS_URL -u AS_TOKEN cargo fmt --check
env -u DS_URL -u DS_TOKEN -u WT_URL -u WT_TOKEN -u WS_URL -u WS_TOKEN -u AS_URL -u AS_TOKEN cargo clippy --all-targets --locked -- -D warnings
env -u DS_URL -u DS_TOKEN -u WT_URL -u WT_TOKEN -u WS_URL -u WS_TOKEN -u AS_URL -u AS_TOKEN cargo test --locked
```

The tests use tmux servers of their own. Without `WORKER_SERVICE_URL`,
`tests/worker_service.rs` prints `skipped:` and passes, so `cargo test` proves
nothing about beta.

## Beta: the integration test

Against beta only, never prod. Its URL is stack `WorkerService-beta`'s
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

## Prod read-only checks

What hosts install now, and that it runs here:

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

## Release to prod (reviewer only)

At the merged commit on `main`, clean tree, after the checks and the beta test
pass there. The tag is numbered one past the last release, and the build is
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

## Roll back (reviewer only)

Find the last good release:

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
