---
name: test
description: How to run agent-host's checks (typecheck, test, build) and its opt-in real-Claude test. Use before submitting a change to agent-host, and when reviewing one.
---

# Testing agent-host

agent-host is a Node 24 TypeScript package. Node runs `src/*.ts` directly, so
nothing is built for development; `build` compiles `dist/` for an install.

## Setup

Node 24 or later and npm on PATH, then, in your clone:

```sh
npm ci
```

## The checks

An author runs all three at the PR's head before submitting it, and a reviewer
at the commit reviewed. Each must exit 0. Run each on its own, with the
session's prod `*_URL`/`*_TOKEN` and `CLAUDE_CONFIG_DIR` unset, and read its
exit status (`echo $?`); never pipe one into `tail` or `head`, whose status
the pipe reports instead:

```sh
env -u CLAUDE_CONFIG_DIR -u DS_URL -u DS_TOKEN -u WT_URL -u WT_TOKEN -u WS_URL -u WS_TOKEN -u AS_URL -u AS_TOKEN -u JS_URL -u JS_TOKEN -u AG_URL -u AG_TOKEN npm run typecheck
env -u CLAUDE_CONFIG_DIR -u DS_URL -u DS_TOKEN -u WT_URL -u WT_TOKEN -u WS_URL -u WS_TOKEN -u AS_URL -u AS_TOKEN -u JS_URL -u JS_TOKEN -u AG_URL -u AG_TOKEN npm test
env -u CLAUDE_CONFIG_DIR -u DS_URL -u DS_TOKEN -u WT_URL -u WT_TOKEN -u WS_URL -u WS_TOKEN -u AS_URL -u AS_TOKEN -u JS_URL -u JS_TOKEN -u AG_URL -u AG_TOKEN npm run build
```

`npm test` runs every `src/*.spec.ts` against a stand-in for the Agent SDK
(`src/testing.ts`) and a local `ws` server for the socket; it starts no
Claude session and reaches nothing off the machine. `src/real.spec.ts` is
skipped in it.

## A real session (by hand, costs a little usage)

`src/real.spec.ts` runs one real Agent SDK session on the machine's Claude
login with a one-word prompt, and checks its lines and state, and that the
installed SDK is `CHECKED_SDK` in `src/claude.ts`. Run it for a change to how
the host speaks to the SDK, and after bumping the SDK; once it passes on a new
release, that release is the new `CHECKED_SDK`. `CLAUDE_CONFIG_DIR` is unset
so the session uses the user's own login:

```sh
AGENT_HOST_REAL_CLAUDE=1 env -u CLAUDE_CONFIG_DIR -u DS_URL -u DS_TOKEN -u WT_URL -u WT_TOKEN -u WS_URL -u WS_TOKEN -u AS_URL -u AS_TOKEN -u JS_URL -u JS_TOKEN -u AG_URL -u AG_TOKEN npx vitest run src/real.spec.ts
```

A pass: `Tests  2 passed (2)`, exit 0. Without `AGENT_HOST_REAL_CLAUDE=1` it
says `2 skipped`, which proves nothing.
