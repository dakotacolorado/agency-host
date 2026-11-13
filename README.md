# agent-host

One process per machine. It keeps a socket open to `agent-service`: every
command comes down it, and every report and heartbeat goes up it. It makes
agents' folders, runs their Claude Code sessions on the Claude Agent SDK
(`@anthropic-ai/claude-agent-sdk`, checked against 0.3.323), carries
messages to them and what they say back.

## Install

On a mac or linux machine with Node 24 or later, `gh` signed in, and the
machine's Claude subscription signed in (`claude` once, by hand): no API key
is needed, and the SDK brings its own Claude Code.

```sh
git clone https://github.com/dakotacolorado/agent-host && cd agent-host
npm ci --omit=dev
node src/cli.ts 2>> ~/agent-host.log &          # Node runs the TypeScript itself
```

or, built: `npm ci && npm run build && npm prune --omit=dev`, then
`node dist/cli.js`. Restart it onto a newer checkout: each session that was
live is resumed (see Restarts).

On linux, a systemd user unit can keep it running,
`~/.config/systemd/user/agent-host.service`, enabled with `systemctl --user
enable --now agent-host`:

```ini
[Unit]
Description=agent-host: this machine's place among agent-service's hosts
After=network-online.target
Wants=network-online.target

[Service]
WorkingDirectory=%h/agent-host
ExecStart=/usr/bin/env node dist/cli.js
Restart=always
RestartSec=5
Environment=PATH=%h/.local/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin

[Install]
WantedBy=default.target
```

Its config (`~/.config/agent-host/`), folders (`~/agents`), lock and log are
all its own, so it can run beside other services on the same machine.

## Configure

`~/.config/agent-host/config.toml` (or `--config PATH`, or
`AGENT_HOST_CONFIG`) holds every setting:

```toml
service_url = "https://…"
token = "…"
host_id = "studio"                 # default: the hostname
capacity = 2                       # unset, nothing is placed here
labels = ["macos"]
agents_dir = "/Users/me/agents"    # default: ~/agents

[permissions]                      # what a session may run when it asks
allow = ["*"]                      # the default: every tool
deny = ["WebFetch", "mcp__github__merge*"]
```

A session's permission requests come to the host (the SDK's `canUseTool`),
which answers each by `[permissions]`: a tool named in `deny` is refused, then
one named in `allow` is given, any other refused; a name ending in `*` is
every tool it begins. Claude Code's own rules in the agent's settings are
checked first, and decide what it never asks about.

Two hosts on one machine (beta beside prod) each need their own `agents_dir`;
one host holds each, by a unix socket in it (`.agent-host.sock`), and a
second is refused. `agent-host --version` prints package.json's version,
which is also the `version` its heartbeats send.

## Socket

`<service_url, http→ws and https→wss>/socket?hostId=<host_id>`, with
`Authorization: Bearer <token>`. On every socket opened the host sends first
a full `report` (every agent, every line and result not yet written on a
socket), then a `heartbeat` (`version`, `labels`, `capacity`), and a heartbeat
every 30 s after. A report goes up whenever something changes, at most every
250 ms, each held to 120 KB and written in frames of 31 KB; lines that do not
fit go in the next. A socket that closes, or hears nothing for 90 s, is opened
again after 1 s, doubling to 60 s, reset once one has stayed open a minute.

Commands come down as `{type: "command", command}`, and may come more than
once: one is done once by its `id`, and one sent again after it is done is
answered again. They are done one at a time, in the order they came.

The usage is the newest `rate_limit_event` any session's stream has said and
no report has yet carried: its 5-hour and weekly windows, each left out when
the event does not carry it, read at the event's time. Tokens are each
session's running totals of its results' `model_usage_since_last_result`
(its subagents' included), told again on each new socket.

## Agents

A `create` makes `<agents_dir>/<name>/`:

```text
agent.json        id and name
sessions.jsonl    each session started, and each stopped or ended
prompt.md         the opening message of the latest session
credentials.env   the latest start's variables and tokens, readable by the host's user alone
mcp.json          the latest start's MCP servers, their ${VAR}s unfilled
home/.claude/     its own Claude Code (CLAUDE_CONFIG_DIR): settings, CLAUDE.md, skills, transcripts
logs/<id>.jsonl   every SDK message each session gave, as it came
logs/<id>.log     everything each session wrote on its stderr
```

A `start` writes its `context` (agent-service's own clients) and `documents`
(honed skills), files under `skills/`, into `home/.claude/`, in place of what
the last start wrote (listed in `home/.claude/.context.json`); a start with
neither keeps the last. The host's own rules are the agent's `CLAUDE.md`. Its
`env` and `secrets` are written to `credentials.env`, mode 600, and set in
the session's environment from there, never on a command line, so a session
resumed after a restart has them again; its `mcpServers` go to the session
with their `${VAR}`s filled from that environment.

A session runs in the agent's folder, a new conversation under the start's
session id or, with `resume`, the last one carried on. Each session shares
the host user's Claude login: HOME stays the user's, and
`CLAUDE_SECURESTORAGE_CONFIG_DIR` points at the user's own store (see
`src/claude.ts`). An agent's settings deny the `AskUserQuestion`,
`EnterPlanMode` and `ExitPlanMode` tools, which wait on a person at a
terminal.

States: `working` (a turn in flight), `waiting` (live, between turns), `idle`
(no session), `interrupted` (its session ended without a stop; can be
resumed). With each go `tasks`, the background tasks and subagents its live
session has open (from the SDK's latest `background_tasks_changed`), and
`told`, whether its turn or its last began with a message the host delivered.
A `message` is pushed into the live session at once and reported done; to an
agent with no live session it is refused. An `interrupt` stops the turn, as
Escape does, then says its text if it has one. `stop` ends the session;
`delete` stops it and moves the folder to
`<agents_dir>/.deleted/<name>-<id>/`. The `instruction` line of a message, a
start's text or an interrupt's carries that command's `commandId`.

To take a session over by hand, stop the agent, then in its folder:

```sh
CLAUDE_CONFIG_DIR=<agents_dir>/<name>/home/.claude claude --resume <id>
```

where `<id>` is the `claude` of its last line in `sessions.jsonl`. A `start`
with `resume` hands it back to the host.

## Restarts

A session does not outlive its host: it is the host's child, and ends when the
host goes. A host that starts again resumes each session that was live when it
went and tells it the host restarted; one cut off before Claude Code wrote
anything is begun again as it was first; one that cannot be begun is reported
`interrupted`. What costs is the turn in flight, which is cut off. A session
that was stopped, or that ended by itself while the host ran, is left as it
was. Every line is read again from the agent's logs, so a restarted host
numbers its lines on, and the service keeps each `seq` once.

Start the host from your own login, not from inside an agent's session: the
host takes its own `CLAUDE_CONFIG_DIR` to be where your Claude login is, and
ignores one inside `agents_dir`.

The folders, logs and records are the Rust host's shapes, so a folder either
wrote is read by the other.

## Develop

The `test` skill (`.claude/skills/test/SKILL.md`) says how to run the checks:

```sh
npm ci
npm run typecheck
npm test
npm run build
AGENT_HOST_REAL_CLAUDE=1 npx vitest run src/real.spec.ts   # one real session
```
