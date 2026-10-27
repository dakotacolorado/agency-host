# agent-host

One static binary per machine. It beats to `agent-service`, starts and stops
agents, carries messages to them and their output back.

Installing it is copying the file.

## Install

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
```

Which service a host beats to is only this, so the same binary tries a change
against beta before any machine is asked to take it:

```sh
AGENT_HOST_SERVICE_URL=https://beta… AGENT_HOST_TOKEN=… agent-host --beats 3
```

## Run

`agent-host` runs until it is killed, and outlives the terminal that started
it. A beat that fails is retried; a machine that sleeps simply beats late.

## Develop

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

CI runs those, builds every target, runs the linux binary through a whole beat
cycle against the beta service, and on a merge to main publishes the release.
