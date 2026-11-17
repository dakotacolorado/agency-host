import { existsSync, mkdirSync, readFileSync, statSync, unlinkSync, utimesSync, writeFileSync, appendFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { Agents, CARRY_ON, DRAINED, NOTHING_YET, RESUMED, START, type Cursor, type Line } from "./agents.ts";
import { EXPLORE } from "./claude.ts";
import { EVERY_TOOL, type Permissions } from "./config.ts";
import { FakeSdk, LIMIT_STOP, scratch, settle, type FakeSession } from "./testing.ts";

const AGENT = "3b0f6c1e-9a52-4c1d-8f43-2a6e1c7d9b10";
const S1 = "9d6a1f0e-4b7c-4e21-a3f8-0c5d2e7b4a19";
const S2 = "1d6a1f0e-4b7c-4e21-a3f8-0c5d2e7b4a20";
const C1 = "aaaaaaaa-0000-4000-8000-000000000001";
const C2 = "aaaaaaaa-0000-4000-8000-000000000002";

function host(dir = scratch("agents"), permissions: Permissions = EVERY_TOOL) {
  const sdk = new FakeSdk();
  let changes = 0;
  const agents = new Agents({ dir, startSession: sdk.start, permissions, login: null, baseEnv: { PATH: "/usr/bin", HOME: "/home/me" }, changed: () => changes++ });
  return { dir, sdk, agents, changes: () => changes };
}

/** Everything the agent has said, read as a host reads it, and its report after. */
function readAll(agents: Agents, id = AGENT, from: Cursor = START) {
  const { home } = agents.all().find((a) => a.record.id === id)!;
  const lines: Line[] = [];
  let at = from;
  const tokens: { sessionId: string; tokens: unknown }[] = [];
  let usage;
  for (let batch = agents.read(home, at, 1 << 20, 1000); batch; batch = agents.read(home, at, 1 << 20, 1000)) {
    lines.push(...batch.lines);
    tokens.push(...batch.tokens);
    usage = batch.usage ?? usage;
    at = batch.next;
  }
  return { lines, at, tokens, usage, report: agents.report(home, id, at) };
}

const shown = (lines: Line[]) => lines.map((l) => [l.seq, l.from, l.text, l.commandId ?? null]);

describe("creating an agent", () => {
  it("makes its folder and its own Claude Code with the host's rules, and is idle", () => {
    const { dir, agents } = host();
    agents.create(AGENT, "ana");
    agents.create(AGENT, "ana");
    expect(JSON.parse(readFileSync(join(dir, "ana/agent.json"), "utf8"))).toEqual({ id: AGENT, name: "ana" });
    expect(readFileSync(join(dir, "ana/home/.claude/CLAUDE.md"), "utf8")).toContain(`You are agent \`ana\`, and your folder is \`${join(dir, "ana")}\``);
    const explore = readFileSync(join(dir, "ana/home/.claude/agents/Explore.md"), "utf8");
    expect(explore).toBe(EXPLORE);
    expect(explore).toMatch(/^---\nname: Explore\n[^]*\nmodel: haiku\ndisallowedTools: Write, Edit, NotebookEdit\n---\n/);
    expect(readAll(agents).report).toEqual({ id: AGENT, state: "idle", sessionId: null, tasks: 0, limitedUntil: null });
    expect(() => agents.create(AGENT, "bob")).toThrow(/here already, named ana/);
    expect(() => agents.create("w2", "Ana")).toThrow(/not an agent name/);
  });
});

describe("starting a session", () => {
  it("writes its context, documents, variables and servers before it runs, and records it first", () => {
    const { dir, sdk, agents } = host();
    agents.create(AGENT, "ana");
    agents.start({
      commandId: C1,
      agentId: AGENT,
      sessionId: S1,
      resume: false,
      text: "Do the work.",
      context: { key: "agency", version: 1, files: [{ path: "skills/page/SKILL.md", content: "page", executable: false }] },
      documents: [{ key: "agent-roles/planner", version: 3, files: [{ path: "skills/planner/SKILL.md", content: "plan", executable: false }] }],
      env: { WT_URL: "https://wt", CLAUDE_CONFIG_DIR: "/not/this" },
      secrets: { WT_TOKEN: "s3cret" },
      mcpServers: { tracker: { type: "http", url: "${WT_URL}/mcp", headers: { Authorization: "Bearer ${WT_TOKEN}" } } },
    });
    const state = join(dir, "ana/home/.claude");
    expect(readFileSync(join(state, "skills/page/SKILL.md"), "utf8")).toBe("page");
    expect(readFileSync(join(state, "skills/planner/SKILL.md"), "utf8")).toBe("plan");
    const rules = readFileSync(join(state, "CLAUDE.md"), "utf8");
    expect(rules).toContain(`You are agent \`ana\``);
    expect(rules).toContain("From honed's `agent-roles/planner`, v3, also at `skills/planner/`:\n\nplan\n");
    expect(rules).not.toContain("page");
    expect(readFileSync(join(dir, "ana/prompt.md"), "utf8")).toBe("Do the work.");
    expect(statSync(join(dir, "ana/credentials.env")).mode & 0o777).toBe(0o600);
    // No token on disk but credentials.env.
    expect(readFileSync(join(dir, "ana/mcp.json"), "utf8")).not.toContain("s3cret");
    expect(readFileSync(join(dir, "ana/sessions.jsonl"), "utf8")).toContain(`"sessionId":"${S1}"`);
    const o = sdk.last.options;
    expect(o).toMatchObject({ cwd: join(dir, "ana"), sessionId: S1 });
    expect(o.resume).toBeUndefined();
    expect(o.env).toMatchObject({ PATH: "/usr/bin", WT_URL: "https://wt", WT_TOKEN: "s3cret", CLAUDE_CONFIG_DIR: state, CLAUDE_SECURESTORAGE_CONFIG_DIR: "" });
    expect(o.mcpServers).toEqual({ tracker: { type: "http", url: "https://wt/mcp", headers: { Authorization: "Bearer s3cret" } } });
    expect(sdk.last.sent).toEqual([{ text: "Do the work.", uuid: C1 }]);
  });

  it("replaces the last start's files, keeps them for a start that carries none, and takes away its servers", () => {
    const { dir, agents } = host();
    agents.create(AGENT, "ana");
    const skill = (name: string) => ({ key: name, version: 1, files: [{ path: `skills/${name}/SKILL.md`, content: name, executable: false }] });
    agents.start({ agentId: AGENT, sessionId: S1, resume: false, context: skill("a"), documents: [skill("b")], mcpServers: { x: { type: "http", url: "u", headers: {} } } });
    return agents.stop(AGENT).then(() => {
      agents.start({ agentId: AGENT, sessionId: S2, resume: true });
      const state = join(dir, "ana/home/.claude");
      expect(existsSync(join(state, "skills/a/SKILL.md")) && existsSync(join(state, "skills/b/SKILL.md"))).toBe(true);
      expect(existsSync(join(dir, "ana/mcp.json"))).toBe(false);
      expect(readFileSync(join(state, "CLAUDE.md"), "utf8")).toContain("From honed's `b`, v1");
      return agents.stop(AGENT).then(() => {
        agents.start({ agentId: AGENT, sessionId: "s3", resume: true, documents: [skill("c")] });
        expect(existsSync(join(state, "skills/a"))).toBe(false);
        expect(existsSync(join(state, "skills/b"))).toBe(false);
        expect(existsSync(join(state, "skills/c/SKILL.md"))).toBe(true);
        expect(readFileSync(join(state, "CLAUDE.md"), "utf8")).toContain("From honed's `c`, v1, also at `skills/c/`:\n\nc\n");
        expect(readFileSync(join(state, "CLAUDE.md"), "utf8")).not.toContain("`b`");
      });
    });
  });

  it("opens with words when it was given none, and carries on the last conversation when resumed", async () => {
    const { sdk, agents } = host();
    agents.create(AGENT, "ana");
    agents.start({ agentId: AGENT, sessionId: S1, resume: false });
    expect(sdk.last.sent[0]!.text).toBe(NOTHING_YET);
    await agents.stop(AGENT);
    agents.start({ agentId: AGENT, sessionId: S2, resume: true });
    expect(sdk.last.sent[0]!.text).toBe(CARRY_ON);
    expect(sdk.last.options.resume).toBe(S1);
    expect(sdk.last.options.sessionId).toBeUndefined();
  });

  it("is done once: sent again it is answered, and another while one runs is refused", async () => {
    const { sdk, agents } = host();
    agents.create(AGENT, "ana");
    agents.start({ agentId: AGENT, sessionId: S1, resume: false, text: "go" });
    agents.start({ agentId: AGENT, sessionId: S1, resume: false, text: "go" });
    expect(sdk.sessions).toHaveLength(1);
    expect(() => agents.start({ agentId: AGENT, sessionId: S2, resume: false })).toThrow(/already has session .* running: stop it first/);
    await agents.stop(AGENT);
    agents.start({ agentId: AGENT, sessionId: S1, resume: false, text: "go" });
    expect(sdk.sessions).toHaveLength(1);
    expect(() => agents.start({ agentId: "nobody", sessionId: S2, resume: false })).toThrow(/no agent nobody on this host/);
  });

  it("that cannot run is taken back, so the start is tried again when it is sent again", () => {
    const { sdk, agents } = host();
    agents.create(AGENT, "ana");
    sdk.refuse = "Claude Code is not installed";
    expect(() => agents.start({ agentId: AGENT, sessionId: S1, resume: false })).toThrow(/not installed/);
    expect(readAll(agents).report.state).toBe("idle");
    sdk.refuse = null;
    agents.start({ agentId: AGENT, sessionId: S1, resume: false });
    expect(sdk.sessions).toHaveLength(1);
  });
});

describe("what a session says", () => {
  it("is lines numbered across sessions, each told line naming its command", async () => {
    const { sdk, agents } = host();
    agents.create(AGENT, "ana");
    agents.start({ commandId: C1, agentId: AGENT, sessionId: S1, resume: false, text: "first" });
    agents.message(AGENT, C2, "second");
    sdk.last.emit({ type: "assistant", message: { content: [{ type: "tool_use", name: "Bash", input: { command: "ls" } }] }, parent_tool_use_id: null });
    await settle();
    await agents.stop(AGENT);
    agents.start({ agentId: AGENT, sessionId: S2, resume: true });
    await settle();
    const { lines } = readAll(agents);
    expect(shown(lines)).toEqual([
      [0, "instruction", "first", C1],
      [1, "agent", "said: first", null],
      [2, "instruction", "second", C2],
      [3, "agent", "said: second", null],
      [4, "tool", 'Bash {"command":"ls"}', null],
      [5, "instruction", CARRY_ON, null],
      [6, "agent", `said: ${CARRY_ON}`, null],
    ]);
    expect(lines.map((l) => l.sessionId)).toEqual([S1, S1, S1, S1, S1, S2, S2]);
    expect(lines.every((l) => /Z$/.test(l.at))).toBe(true);
  });

  it("reads the same again from the start, as a host that restarted does, and a read put back reads the same", async () => {
    const { sdk, agents } = host();
    agents.create(AGENT, "ana");
    agents.start({ commandId: C1, agentId: AGENT, sessionId: S1, resume: false, text: "first" });
    await settle();
    const once = readAll(agents);
    const { home } = agents.all()[0]!;
    const one = agents.read(home, START, 1, 1000)!;
    expect(one.lines).toHaveLength(1);
    expect(agents.read(home, START, 1, 1000)).toEqual(one);
    expect(readAll(agents, AGENT, one.next).lines).toEqual(once.lines.slice(1));
    expect(sdk.sessions).toHaveLength(1);
  });

  it("leaves a message being written for the next read, and passes over one an ended session left torn", async () => {
    const { dir, sdk, agents } = host();
    agents.create(AGENT, "ana");
    agents.start({ agentId: AGENT, sessionId: S1, resume: false, text: "hi" });
    await settle();
    const events = join(dir, "ana/logs", `${S1}.jsonl`);
    appendFileSync(events, '{"type":"assistant","message":{"content":[{"type":"text","text":"half');
    const before = readAll(agents);
    appendFileSync(events, ' and the rest"}]}}\n');
    expect(shown(readAll(agents, AGENT, before.at).lines)).toEqual([[2, "agent", "half and the rest", null]]);
    appendFileSync(events, '{"type":"assistant","message":{"content":[{"type":"text","text":"torn');
    await agents.stop(AGENT);
    agents.start({ agentId: AGENT, sessionId: S2, resume: true, text: "next" });
    await settle();
    expect(shown(readAll(agents).lines).slice(3)).toEqual([[3, "instruction", "next", null], [4, "agent", "said: next", null]]);
    expect(sdk.sessions).toHaveLength(2);
  });
});

describe("an agent's state", () => {
  it("is working in a turn, waiting between turns, told when a delivered message began it, and idle once stopped", async () => {
    const { sdk, agents } = host();
    agents.create(AGENT, "ana");
    agents.start({ commandId: C1, agentId: AGENT, sessionId: S1, resume: false, text: "go" });
    const s = sdk.last;
    s.onSend = () => {};
    expect(readAll(agents).report).toMatchObject({ state: "working", sessionId: S1 });
    await settle();
    expect(readAll(agents).report).toEqual({ id: AGENT, state: "waiting", sessionId: S1, tasks: 0, told: true, limitedUntil: null, turns: 1, activeMs: 0 });
    // A turn a background task's end begins is not told.
    s.emit({ type: "system", subtype: "init" });
    await settle();
    expect(readAll(agents).report).toEqual({ id: AGENT, state: "working", sessionId: S1, tasks: 0, told: false, limitedUntil: null, turns: 1, activeMs: 0 });
    s.emit({ type: "system", subtype: "background_tasks_changed", tasks: [{ task_id: "a" }, { task_id: "b" }] });
    s.emit({ type: "result", subtype: "success" });
    await settle();
    expect(readAll(agents).report).toEqual({ id: AGENT, state: "waiting", sessionId: S1, tasks: 2, told: false, limitedUntil: null, turns: 2, activeMs: 0 });
    await agents.stop(AGENT);
    expect(s.closed).toBe(true);
    expect(readAll(agents).report).toMatchObject({ state: "idle", sessionId: null, tasks: 0 });
  });

  it("is interrupted when its session dies without a stop, and its tasks gone", async () => {
    const { sdk, agents } = host();
    agents.create(AGENT, "ana");
    agents.start({ agentId: AGENT, sessionId: S1, resume: false, text: "go" });
    sdk.last.emit({ type: "system", subtype: "background_tasks_changed", tasks: [{ task_id: "a" }] });
    await settle();
    sdk.last.fail();
    await settle();
    expect(agents.isLive(AGENT)).toBe(false);
    expect(readAll(agents).report).toMatchObject({ state: "interrupted", sessionId: S1, tasks: 0 });
    expect(() => agents.message(AGENT, C2, "hello?")).toThrow(`agent ${AGENT} has no live session on this host`);
  });

  it("goes on waiting after an interrupt, told what the interrupt says", async () => {
    const { sdk, agents } = host();
    agents.create(AGENT, "ana");
    agents.start({ agentId: AGENT, sessionId: S1, resume: false, text: "go" });
    const s = sdk.last;
    s.onSend = () => {};
    await agents.interrupt(AGENT, C2, "Stop that; do this.");
    expect(s.interrupts).toBe(1);
    expect(s.sent.at(-1)).toEqual({ text: "Stop that; do this.", uuid: C2 });
    await agents.interrupt(AGENT, "c3", undefined);
    expect(s.sent).toHaveLength(2);
    await agents.stop(AGENT);
    await expect(agents.interrupt(AGENT, "c4", undefined)).rejects.toThrow(/no live session/);
  });
});

describe("tokens and usage", () => {
  it("are each session's running totals of its results, and the newest reading of the account's windows", async () => {
    const { sdk, agents } = host();
    agents.create(AGENT, "ana");
    agents.start({ agentId: AGENT, sessionId: S1, resume: false, text: "one" });
    agents.message(AGENT, C1, "two");
    sdk.last.emit({ type: "rate_limit_event", rate_limit_info: { unifiedWindows: { five_hour: { utilization: 0.2, resetsAt: 1794573600 } } }, timestamp: "2026-11-13T08:00:00.000Z" });
    sdk.last.emit({ type: "rate_limit_event", rate_limit_info: { unifiedWindows: { seven_day: { utilization: 0.8, resetsAt: 1794668400 } } }, timestamp: "2026-11-13T08:01:00.000Z" });
    await settle();
    await agents.stop(AGENT);
    agents.start({ agentId: AGENT, sessionId: S2, resume: true, text: "three" });
    await settle();
    const { tokens, usage } = readAll(agents);
    const total = (n: number) => ({ inputTokens: n, outputTokens: 10 * n, cacheCreationInputTokens: 0, cacheReadInputTokens: 100 * n });
    expect(tokens.at(-2)).toEqual({ sessionId: S1, tokens: total(2), byModel: { m: total(2) } });
    expect(tokens.at(-1)).toEqual({ sessionId: S2, tokens: total(1), byModel: { m: total(1) } });
    expect(usage).toEqual({ readAt: "2026-11-13T08:01:00.000Z", sevenDay: { utilization: 0.8, resetsAt: "2026-11-14T15:00:00.000Z" } });
  });
});

const RESETS = "2026-11-15T08:50:00.000Z";

describe("a usage limit", () => {
  it("that ended a turn is reported until the next turn begins, by a message delivered or by itself", async () => {
    const { sdk, agents } = host();
    agents.create(AGENT, "ana");
    agents.start({ commandId: C1, agentId: AGENT, sessionId: S1, resume: false, text: "go" });
    await settle();
    expect(readAll(agents).report.limitedUntil).toBeNull();
    const s = sdk.last;
    s.onSend = () => {};
    for (const m of LIMIT_STOP) s.emit(m);
    await settle();
    expect(readAll(agents).report).toMatchObject({ state: "waiting", limitedUntil: RESETS });
    agents.message(AGENT, C2, "Carry on.");
    expect(readAll(agents).report.limitedUntil).toBeNull();
    for (const m of LIMIT_STOP) s.emit(m);
    await settle();
    expect(readAll(agents).report.limitedUntil).toBe(RESETS);
    s.emit({ type: "system", subtype: "init" });
    await settle();
    expect(readAll(agents).report.limitedUntil).toBeNull();
  });

  it("is not reported for a turn with only allowed readings, or one that went on after a refusal", async () => {
    const { sdk, agents } = host();
    agents.create(AGENT, "ana");
    agents.start({ agentId: AGENT, sessionId: S1, resume: false, text: "go" });
    await settle();
    const s = sdk.last;
    const [refused] = LIMIT_STOP;
    s.emit({ type: "system", subtype: "init" });
    for (const status of ["allowed", "allowed_warning"]) {
      s.emit({ type: "rate_limit_event", rate_limit_info: { status, resetsAt: 1794732600, unifiedWindows: { five_hour: { utilization: 0.9, resetsAt: 1794732600, status } } } });
    }
    s.emit({ type: "result", subtype: "success" });
    await settle();
    expect(readAll(agents).report).toMatchObject({ state: "waiting", limitedUntil: null });
    s.emit({ type: "system", subtype: "init" });
    s.emit(refused!);
    s.emit({ type: "assistant", message: { model: "claude-opus-5-5", content: [{ type: "text", text: "on I go" }], stop_reason: "end_turn" } });
    s.emit({ type: "result", subtype: "success" });
    await settle();
    expect(readAll(agents).report).toMatchObject({ state: "waiting", limitedUntil: null });
  });

  it("is reported again by a host restarted while it holds, and is gone with a stop", async () => {
    const first = host();
    first.agents.create(AGENT, "ana");
    first.agents.start({ commandId: C1, agentId: AGENT, sessionId: S1, resume: false, text: "go" });
    await settle();
    for (const m of LIMIT_STOP) first.sdk.last.emit(m);
    await settle();
    const state = join(first.dir, "ana/home/.claude");
    mkdirSync(join(state, "projects/-x"), { recursive: true });
    writeFileSync(join(state, "projects/-x", `${S1}.jsonl`), "{}\n");

    const again = host(first.dir);
    expect(readAll(again.agents).report).toMatchObject({ sessionId: S1, limitedUntil: RESETS });
    // Told it restarted, its session is resumed into a turn of its own, which a limit that still holds ends again.
    again.agents.reconcile();
    expect(again.sdk.last.sent[0]!.text).toBe(RESUMED);
    expect(readAll(again.agents).report).toMatchObject({ state: "working", sessionId: S1, limitedUntil: RESETS });
    await again.agents.stop(AGENT);
    expect(readAll(again.agents).report).toMatchObject({ state: "idle", limitedUntil: null });
  });
});

describe("permissions", () => {
  it("answers each request by the host's rule", () => {
    const { sdk, agents } = host(scratch("permissions"), { allow: ["Read", "mcp__github__*"], deny: ["mcp__github__merge*"] });
    agents.create(AGENT, "ana");
    agents.start({ agentId: AGENT, sessionId: S1, resume: false });
    const permit = sdk.last.options.permit;
    expect(permit("Read")).toBe(true);
    expect(permit("mcp__github__create_pr")).toBe(true);
    expect(permit("mcp__github__merge_pr")).toBe("mcp__github__merge_pr is not allowed on this host (agency-host's permissions)");
    expect(permit("Bash")).toMatch(/not allowed/);
  });
});

describe("the model a session runs on", () => {
  it("is the one its start names, with its effort, new or resumed; a start that names none runs on Claude Code's defaults", async () => {
    const { sdk, agents } = host();
    agents.create(AGENT, "ana");
    agents.start({ agentId: AGENT, sessionId: S1, resume: false, model: "claude-haiku-5-5", effort: "low" });
    expect(sdk.last.options).toMatchObject({ model: "claude-haiku-5-5", effort: "low" });
    await agents.stop(AGENT);
    agents.start({ agentId: AGENT, sessionId: S2, resume: true, model: "claude-sonnet-5-5" });
    expect(sdk.last.options).toMatchObject({ resume: S1, model: "claude-sonnet-5-5" });
    expect(sdk.last.options.effort).toBeUndefined();
    await agents.stop(AGENT);
    agents.start({ agentId: AGENT, sessionId: "s3", resume: true });
    expect(sdk.last.options.model).toBeUndefined();
    expect(sdk.last.options.effort).toBeUndefined();
  });

  it("is switched live by configure, with no restart, and a host that restarts resumes it on the new one", async () => {
    const first = host();
    first.agents.create(AGENT, "ana");
    first.agents.start({ agentId: AGENT, sessionId: S1, resume: false, text: "go", model: "claude-haiku-5-5" });
    await settle();
    await first.agents.configure(AGENT, "claude-sonnet-5-5", "high");
    expect(first.sdk.sessions).toHaveLength(1);
    expect(first.sdk.last.configured).toEqual([{ model: "claude-sonnet-5-5", effort: "high" }]);
    expect(first.sdk.last.closed).toBe(false);
    await first.agents.configure(AGENT, "claude-sonnet-5-5", null);
    expect(first.sdk.last.configured.at(-1)).toEqual({ model: "claude-sonnet-5-5", effort: null });
    const state = join(first.dir, "ana/home/.claude");
    mkdirSync(join(state, "projects/-x"), { recursive: true });
    writeFileSync(join(state, "projects/-x", `${S1}.jsonl`), "{}\n");
    unlinkSync(join(state, "agents/Explore.md"));

    const again = host(first.dir);
    again.agents.reconcile();
    expect(again.sdk.last.options).toMatchObject({ resume: S1, model: "claude-sonnet-5-5" });
    expect(again.sdk.last.options.effort).toBeUndefined();
    // An agent made before the host wrote its Explore is given it as its session is resumed.
    expect(readFileSync(join(state, "agents/Explore.md"), "utf8")).toBe(EXPLORE);
  });

  it("is recorded by configure for a session a drain closed, which resumes on it; with no session, configure says so", async () => {
    const { sdk, agents } = host();
    agents.create(AGENT, "ana");
    await expect(agents.configure(AGENT, "claude-haiku-5-5", null)).rejects.toThrow(/no live session on this host: its next start names the model/);
    agents.start({ agentId: AGENT, sessionId: S1, resume: false, text: "go", model: "claude-opus-5-5" });
    await settle();
    await agents.drain();
    await agents.configure(AGENT, "claude-haiku-5-5", "medium");
    expect(sdk.last.configured).toEqual([]);
    agents.message(AGENT, C2, "more");
    expect(sdk.sessions).toHaveLength(2);
    expect(sdk.last.options).toMatchObject({ resume: S1, model: "claude-haiku-5-5", effort: "medium" });
  });
});

describe("the context size", () => {
  it("is reported from the main thread's last request, not a subagent's or Claude Code's own notice, and stands once idle", async () => {
    const { sdk, agents } = host();
    agents.create(AGENT, "ana");
    agents.start({ agentId: AGENT, sessionId: S1, resume: false });
    await settle();
    expect(readAll(agents).report.contextTokens).toBeUndefined();
    const assistant = (usage: object, extra: object = {}) =>
      sdk.last.emit({ type: "assistant", message: { model: "claude-haiku-5-5", content: [{ type: "text", text: "hi" }], stop_reason: null, usage }, parent_tool_use_id: null, ...extra });
    assistant({ input_tokens: 5, cache_read_input_tokens: 1000, cache_creation_input_tokens: 200, output_tokens: 50 });
    assistant({ input_tokens: 3, cache_read_input_tokens: 1200, cache_creation_input_tokens: 40, output_tokens: 9 });
    assistant({ input_tokens: 99999, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 }, { parent_tool_use_id: "toolu_1" });
    sdk.last.emit({ type: "assistant", message: { model: "<synthetic>", content: [{ type: "text", text: "limit" }], usage: { input_tokens: 0 } }, parent_tool_use_id: null });
    await settle();
    expect(readAll(agents).report.contextTokens).toBe(1243);
    await agents.stop(AGENT);
    expect(readAll(agents).report).toMatchObject({ state: "idle", contextTokens: 1243 });
    // A new session begins with none.
    agents.start({ agentId: AGENT, sessionId: S2, resume: false });
    await settle();
    expect(readAll(agents).report.contextTokens).toBeUndefined();
  });
});

describe("a host that restarts", () => {
  it("resumes each session that was live, told the host restarted, and leaves those stopped or ended", async () => {
    const first = host();
    for (const [id, name] of [[AGENT, "ana"], ["w2", "bob"], ["w3", "cy"], ["w4", "dee"]] as const) first.agents.create(id, name);
    first.agents.start({ commandId: C1, agentId: AGENT, sessionId: S1, resume: false, text: "go" });
    first.agents.start({ agentId: "w2", sessionId: "s-bob", resume: false, text: "go" });
    first.agents.start({ agentId: "w3", sessionId: "s-cy", resume: false, text: "go" });
    await settle();
    await first.agents.stop("w2");
    first.sdk.sessions[2]!.fail();
    await settle();
    // Claude Code wrote ana's transcript; the machine is unplugged.
    const state = join(first.dir, "ana/home/.claude");
    mkdirSync(join(state, "projects/-x"), { recursive: true });
    writeFileSync(join(state, "projects/-x", `${S1}.jsonl`), "{}\n");

    const again = host(first.dir);
    again.agents.reconcile();
    expect(again.sdk.sessions).toHaveLength(1);
    const s = again.sdk.last;
    expect(s.options.resume).toBe(S1);
    expect(s.sent[0]!.text).toBe(RESUMED);
    await settle();
    expect(readAll(again.agents).report).toMatchObject({ state: "waiting", sessionId: S1 });
    expect(readAll(again.agents, "w2").report.state).toBe("idle");
    expect(readAll(again.agents, "w3").report).toMatchObject({ state: "interrupted", sessionId: "s-cy" });
    expect(readAll(again.agents, "w4").report.state).toBe("idle");
    expect(shown(readAll(again.agents).lines)).toEqual([
      [0, "instruction", "go", C1],
      [1, "agent", "said: go", null],
      [2, "instruction", RESUMED, null],
      [3, "agent", `said: ${RESUMED}`, null],
    ]);
  });

  it("begins again as it was first a session cut off before Claude Code wrote anything", async () => {
    const first = host();
    first.agents.create(AGENT, "ana");
    first.sdk.setup = (s) => (s.onSend = () => {});
    first.agents.start({ commandId: C1, agentId: AGENT, sessionId: S1, resume: false, text: "go" });
    const again = host(first.dir);
    again.agents.reconcile();
    expect(again.sdk.last.options).toMatchObject({ sessionId: S1 });
    expect(again.sdk.last.sent).toEqual([{ text: "go", uuid: C1 }]);
    await settle();
    expect(shown(readAll(again.agents).lines)).toEqual([[0, "instruction", "go", C1], [1, "agent", "said: go", null]]);
  });

  it("reports interrupted a session that cannot be resumed", () => {
    const first = host();
    first.agents.create(AGENT, "ana");
    first.agents.start({ agentId: AGENT, sessionId: S1, resume: false, text: "go" });
    const again = host(first.dir);
    again.sdk.refuse = "no Claude Code";
    again.agents.reconcile();
    expect(readAll(again.agents).report).toMatchObject({ state: "interrupted", sessionId: S1 });
    // Not tried again on the next start of the host.
    const third = host(first.dir);
    third.agents.reconcile();
    expect(third.sdk.sessions).toHaveLength(0);
  });

  it("reads on from what the Rust host wrote, its echoes naming their commands", () => {
    const { dir, agents } = host();
    agents.create(AGENT, "ana");
    writeFileSync(join(dir, "ana/sessions.jsonl"), `{"sessionId":"${S1}","claude":"${S1}","from":0,"stream":true,"opening":"${C1}"}\n{"sessionId":"${S1}","stopped":true}\n`);
    mkdirSync(join(dir, "ana/logs"), { recursive: true });
    writeFileSync(
      join(dir, "ana/logs", `${S1}.jsonl`),
      [
        { type: "agent_host", subtype: "process_started", timestamp: "2026-11-12T00:00:00.000Z" },
        { type: "system", subtype: "init", timestamp: "2026-11-12T00:00:01.000Z" },
        { type: "user", message: { role: "user", content: "go" }, uuid: C1, isReplay: true, timestamp: "2026-11-12T00:00:01.000Z" },
        { type: "user", message: { role: "user", content: "carry on" }, uuid: "0d1f2c3b-aaaa-8bbb-9ccc-000000000000", isReplay: true, timestamp: "2026-11-12T00:00:02.000Z" },
      ]
        .map((l) => JSON.stringify(l))
        .join("\n") + "\n",
    );
    expect(shown(readAll(agents).lines)).toEqual([[0, "instruction", "go", C1], [1, "instruction", "carry on", null]]);
  });
});

/** Claude Code has written the agent's transcript, so its session can be resumed. */
function transcript(dir: string, name: string, claudeSession: string): void {
  const projects = join(dir, name, "home/.claude/projects/-x");
  mkdirSync(projects, { recursive: true });
  writeFileSync(join(projects, `${claudeSession}.jsonl`), "{}\n");
}

/** A session that takes each message and begins a turn, which the test ends. */
const working = (s: FakeSession) =>
  (s.onSend = (text, uuid) => {
    s.emit({ type: "system", subtype: "init", session_id: "s" });
    s.emit({ type: "user", message: { role: "user", content: text }, uuid, isReplay: true, parent_tool_use_id: null });
  });
const endTurn = (s: FakeSession) => s.emit({ type: "result", subtype: "success" });

describe("draining", () => {
  it("closes at once a session between turns, as the host going does, and reports it waiting", async () => {
    const { dir, sdk, agents } = host();
    agents.create(AGENT, "ana");
    agents.start({ agentId: AGENT, sessionId: S1, resume: false, text: "go" });
    await settle();
    const drained = agents.drain();
    expect(sdk.last.closed).toBe(true);
    await drained;
    expect(agents.isLive(AGENT)).toBe(false);
    expect(readFileSync(join(dir, "ana/sessions.jsonl"), "utf8")).toContain(`{"sessionId":"${S1}","drained":true}`);
    const latest = agents.all()[0]!.home.latest()!;
    expect(latest).toMatchObject({ stopped: false, ended: false, drained: true });
    expect(readAll(agents).report).toMatchObject({ state: "waiting", sessionId: S1, tasks: 0 });
    // A drain called again is the same, and harmless.
    await agents.drain();
  });

  it("closes a session in a turn, or with a background task open, only once that is over", async () => {
    const { sdk, agents } = host();
    agents.create(AGENT, "ana");
    agents.create("w2", "bob");
    sdk.setup = working;
    agents.start({ agentId: AGENT, sessionId: S1, resume: false, text: "go" });
    agents.start({ agentId: "w2", sessionId: "s-bob", resume: false, text: "go" });
    const [ana, bob] = sdk.sessions as [FakeSession, FakeSession];
    await settle();
    bob.emit({ type: "system", subtype: "background_tasks_changed", tasks: [{ id: "t1" }] });
    endTurn(bob);
    await settle();
    let done = false;
    void agents.drain().then(() => (done = true));
    await settle();
    expect([ana.closed, bob.closed]).toEqual([false, false]);
    // Told something while it worked, which it has not yet taken: its turns are not over.
    ana.onSend = () => {};
    agents.message(AGENT, C2, "and this");
    endTurn(ana);
    await settle();
    expect(ana.closed).toBe(false);
    ana.emit({ type: "attachment", attachment: { type: "queued_command", prompt: "and this" } });
    endTurn(ana);
    await settle();
    expect(ana.closed).toBe(true);
    expect(bob.closed).toBe(false);
    bob.emit({ type: "system", subtype: "background_tasks_changed", tasks: [] });
    await settle();
    expect(bob.closed).toBe(true);
    expect(done).toBe(true);
  });

  it("closes a session started, or a drained one told something, as that turn ends", async () => {
    const { sdk, agents } = host();
    agents.create(AGENT, "ana");
    agents.create("w2", "bob");
    agents.create("w3", "cy");
    agents.start({ agentId: "w3", sessionId: "s-cy", resume: false, text: "go" });
    await settle();
    sdk.setup = working;
    agents.start({ agentId: "w2", sessionId: "s-bob", resume: false, text: "go" });
    await settle();
    let done = false;
    void agents.drain().then(() => (done = true));
    await settle();
    expect(sdk.sessions[0]!.closed).toBe(true);
    agents.start({ agentId: AGENT, sessionId: S1, resume: false, text: "go" });
    agents.message("w3", C1, "one more");
    const [, bob, ana, cy] = sdk.sessions as FakeSession[];
    expect(cy!.options.resume).toBe(claudeOf(agents, "w3"));
    expect(cy!.sent).toEqual([{ text: "one more", uuid: C1 }]);
    await settle();
    expect([bob!.closed, ana!.closed, cy!.closed]).toEqual([false, false, false]);
    for (const s of [bob!, ana!, cy!]) endTurn(s);
    await settle();
    expect([bob!.closed, ana!.closed, cy!.closed]).toEqual([true, true, true]);
    expect(done).toBe(true);
  });

  it("is told by the next host it lost nothing, where one cut off at the deadline is told it was", async () => {
    const first = host();
    first.agents.create(AGENT, "ana");
    first.agents.create("w2", "bob");
    first.agents.start({ agentId: AGENT, sessionId: S1, resume: false, text: "go" });
    first.sdk.setup = working;
    first.agents.start({ agentId: "w2", sessionId: "s-bob", resume: false, text: "go" });
    await settle();
    transcript(first.dir, "ana", claudeOf(first.agents, AGENT));
    transcript(first.dir, "bob", claudeOf(first.agents, "w2"));
    void first.agents.drain();
    await settle();
    // The deadline: what is still live is closed as the host going closes it.
    await first.agents.closeAll();

    const again = host(first.dir);
    again.agents.reconcile();
    const told = new Map(again.sdk.sessions.map((s) => [s.options.cwd, s.sent[0]!.text]));
    expect(told).toEqual(new Map([[join(first.dir, "ana"), DRAINED], [join(first.dir, "bob"), RESUMED]]));
    await settle();
    expect(readAll(again.agents).report).toMatchObject({ state: "waiting", sessionId: S1 });

    // Resumed, it is no longer drained: cut off now, it is told so.
    const third = host(first.dir);
    third.agents.reconcile();
    expect(third.sdk.sessions.map((s) => s.sent[0]!.text)).toEqual([RESUMED, RESUMED]);
  });
});

/** The Claude Code session the agent's latest session carries. */
function claudeOf(agents: Agents, id: string): string {
  return agents.all().find((a) => a.record.id === id)!.home.latest()!.claude;
}

describe("deleting", () => {
  it("stops the agent and moves its folder aside; one not here is done", async () => {
    const { dir, sdk, agents } = host();
    agents.create(AGENT, "ana");
    agents.start({ agentId: AGENT, sessionId: S1, resume: false });
    await agents.delete(AGENT);
    expect(sdk.last.closed).toBe(true);
    expect(agents.all()).toEqual([]);
    expect(existsSync(join(dir, ".deleted", `ana-${AGENT}`, "agent.json"))).toBe(true);
    await agents.delete(AGENT);
    await agents.stop(AGENT);
    expect(() => agents.create(AGENT, "ana")).toThrow(/was deleted here/);
  });

  it("takes the credentials, and keeps an old deleted folder", async () => {
    const { dir, agents } = host();
    const old = join(dir, ".deleted", "bo-old");
    mkdirSync(old, { recursive: true });
    const t = new Date(Date.now() - 30 * 24 * 60 * 60 * 1000);
    utimesSync(old, t, t);
    agents.create(AGENT, "ana");
    agents.start({ agentId: AGENT, sessionId: S1, resume: false, env: { A: "1" } });
    await agents.delete(AGENT);
    await agents.prune();
    expect(existsSync(join(dir, ".deleted", `ana-${AGENT}`, "credentials.env"))).toBe(false);
    expect(statSync(old).isDirectory()).toBe(true);
  });

});

describe("tokens by model, turns and active time", () => {
  const usage = (n: number) => ({ inputTokens: n, outputTokens: 2 * n, cacheCreationInputTokens: 3 * n, cacheReadInputTokens: 4 * n });
  it("reports each model's total, the four counts as their sum, and the turns and their durations, starting again with a new session", async () => {
    const { sdk, agents } = host();
    agents.create(AGENT, "ana");
    agents.start({ agentId: AGENT, sessionId: S1, resume: false });
    await settle();
    const result = (since: object, ms: number) => sdk.last.emit({ type: "result", subtype: "success", duration_ms: ms, model_usage_since_last_result: since });
    result({ "claude-opus-5-5": usage(1) }, 100);
    result({ "claude-sonnet-5-5": usage(2), "claude-opus-5-5": usage(3) }, 250);
    result({ "claude-sonnet-5-5": usage(1) }, 50);
    await settle();
    const r = readAll(agents);
    // The session's own first turn, of the scripted session, is counted too.
    const first = { inputTokens: 1, outputTokens: 10, cacheCreationInputTokens: 0, cacheReadInputTokens: 100 };
    const last = r.tokens.at(-1) as unknown as { tokens: unknown; byModel: unknown };
    expect(last.byModel).toEqual({ m: first, "claude-opus-5-5": usage(4), "claude-sonnet-5-5": usage(3) });
    expect(last.tokens).toEqual({ inputTokens: 8, outputTokens: 24, cacheCreationInputTokens: 21, cacheReadInputTokens: 128 });
    expect(r.report).toMatchObject({ turns: 4, activeMs: 400 });
    await agents.stop(AGENT);
    agents.start({ agentId: AGENT, sessionId: S2, resume: false });
    await settle();
    // Only the new session's own first turn, not the old one's four.
    expect(readAll(agents).report).toMatchObject({ turns: 1, activeMs: 0 });
  });
});
