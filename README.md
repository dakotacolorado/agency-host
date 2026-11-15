# agency-host

```
 █████╗  ██████╗ ███████╗███╗   ██╗ ██████╗██╗   ██╗      ██╗  ██╗ ██████╗ ███████╗████████╗        ▄▄██▄▄
██╔══██╗██╔════╝ ██╔════╝████╗  ██║██╔════╝╚██╗ ██╔╝      ██║  ██║██╔═══██╗██╔════╝╚══██╔══╝    ▄▄██████████▄▄
███████║██║  ███╗█████╗  ██╔██╗ ██║██║      ╚████╔╝ █████╗███████║██║   ██║███████╗   ██║      ▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀
██╔══██║██║   ██║██╔══╝  ██║╚██╗██║██║       ╚██╔╝  ╚════╝██╔══██║██║   ██║╚════██║   ██║       ██  ██  ██  ██
██║  ██║╚██████╔╝███████╗██║ ╚████║╚██████╗   ██║         ██║  ██║╚██████╔╝███████║   ██║       ██  ██  ██  ██
╚═╝  ╚═╝ ╚═════╝ ╚══════╝╚═╝  ╚═══╝ ╚═════╝   ╚═╝         ╚═╝  ╚═╝ ╚═════╝ ╚══════╝   ╚═╝     ▄████████████████▄
v0.3.0

Agents at your command, across all your computers.
agency-host runs agency's agents on this computer, each a Claude Code session on the Claude Agent SDK.
```

**Agents at your command, across all your computers.**

agency-host makes a computer one of [agency](https://github.com/dakotacolorado/agency)'s
hosts. It runs the agents agency places on it, each a Claude Code session on
the Claude Agent SDK, carries messages to them, and reports what they do.

## Run it

You need macOS or Linux, Node 24 or later, `gh` signed in, and Claude signed
in on the machine (run `claude` once by hand). No API key is needed.

```sh
git clone https://github.com/dakotacolorado/agency-host && cd agency-host
npm ci && npm run build && npm prune --omit=dev
node dist/cli.js 2>> ~/agency-host.log &
```

Start it from your own login, not from inside an agent's session. When it
restarts, it resumes each session that was live; the turn in flight is lost.

On Linux, a systemd user unit keeps it running. Save this as
`~/.config/systemd/user/agency-host.service` and run
`systemctl --user enable --now agency-host`:

```ini
[Unit]
Description=agency-host
After=network-online.target
Wants=network-online.target

[Service]
WorkingDirectory=%h/agency-host
ExecStart=/usr/bin/env node dist/cli.js
Restart=always
RestartSec=5
Environment=PATH=%h/.local/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin

[Install]
WantedBy=default.target
```

## Configure it

Every setting is in `~/.config/agency-host/config.toml` (or
`$XDG_CONFIG_HOME/agency-host/config.toml`, or the path given by `--config`
or `AGENCY_HOST_CONFIG`):

```toml
service_url = "https://agency.example"
token = "…"
capacity = 2
labels = ["macos"]

[permissions]
deny = ["WebFetch", "mcp__github__merge*"]
```

| Setting | Default | What it does |
|---|---|---|
| `service_url` | required | agency's address. |
| `token` | required | agency's `AUTH_TOKEN`. |
| `host_id` | the hostname | This host's id in agency. |
| `capacity` | none | How many agents may run here at once. Unset, none are placed here. |
| `labels` | none | Labels requests can ask a host to carry. |
| `agents_dir` | `~/agents` | Where each agent's folder is. Two hosts on one machine need different ones. |
| `permissions.allow` | `["*"]` | Tools a session may use. A name ending in `*` matches every tool it begins. |
| `permissions.deny` | none | Tools a session may not use. Checked before `allow`. |

## Use it

You manage agents through agency, not here. On this machine, each agent has a
folder `<agents_dir>/<name>/`:

```text
agent.json        id and name
sessions.jsonl    each session started, stopped or ended
prompt.md         the opening message of the latest session
credentials.env   the variables and tokens it was given, readable by you alone
home/.claude/     its own Claude Code settings, CLAUDE.md, skills and transcripts
logs/<id>.jsonl   every SDK message of each session
logs/<id>.log     each session's stderr
```

A deleted agent's folder moves to `<agents_dir>/.deleted/<name>-<id>/`,
without its `credentials.env` and `mcp.json`. It is kept for two days, and
only the newest 20 are kept; the host removes the rest when it starts, after
each delete, and hourly, leaving an empty file by the folder's name.

To take a session over by hand, stop the agent in agency, then in its folder:

```sh
CLAUDE_CONFIG_DIR=<agents_dir>/<name>/home/.claude claude --resume <id>
```

where `<id>` is the `claude` field of the last line of `sessions.jsonl`.
Starting the agent from agency hands it back.

## How it works

An agent's state is `working` (a turn in flight), `waiting` (live, between
turns), `idle` (no session) or `interrupted` (its session ended without a
stop; it can be resumed). A message goes into the live session at once; with
no live session it is refused. Sessions cannot use `AskUserQuestion` or plan
mode, which wait on a person at a terminal.

## Develop it

```sh
npm ci
npm run typecheck
npm test
npm run build
AGENCY_HOST_REAL_CLAUDE=1 npx vitest run src/real.spec.ts   # one real session; uses a little usage
```

## License

MIT. See [LICENSE](./LICENSE).
