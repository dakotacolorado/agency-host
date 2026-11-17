/**
 * A real Agent SDK session on this machine's Claude login, with a one-word
 * prompt: skipped unless `AGENCY_HOST_REAL_CLAUDE=1` (or its old name,
 * `AGENT_HOST_REAL_CLAUDE=1`), since it costs usage.
 * Run with `CLAUDE_CONFIG_DIR` unset, so the session uses the user's own login.
 */
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { Agents, START, type Cursor, type Line } from "./agents.ts";
import { CHECKED_SDK, loginDir } from "./claude.ts";
import { EVERY_TOOL } from "./config.ts";
import { startSdkSession } from "./sdk.ts";
import { scratch, until } from "./testing.ts";

const real = process.env.AGENCY_HOST_REAL_CLAUDE === "1" || process.env.AGENT_HOST_REAL_CLAUDE === "1";

describe.skipIf(!real)("a real session", () => {
  it("is the SDK the shapes were checked against", () => {
    const sdk = JSON.parse(readFileSync(new URL("../node_modules/@anthropic-ai/claude-agent-sdk/package.json", import.meta.url), "utf8")) as { version: string };
    expect(sdk.version).toBe(CHECKED_SDK);
  });

  it("says one word on the model its start names, its lines name the start, and it waits; configured, it answers on the new model, then stops", { timeout: 300_000 }, async () => {
    const dir = scratch("real");
    const env = Object.fromEntries(Object.entries(process.env).filter((e): e is [string, string] => e[1] !== undefined && e[0] !== "CLAUDE_CONFIG_DIR"));
    const agents = new Agents({ dir, startSession: startSdkSession, permissions: EVERY_TOOL, login: loginDir(process.env.CLAUDE_CONFIG_DIR, dir), baseEnv: env, changed: () => {} });
    const id = "6a1c2f3e-0000-4000-8000-00000000a9e1";
    const command = "6a1c2f3e-0000-4000-8000-00000000c001";
    agents.create(id, "real");
    const session = "6a1c2f3e-0000-4000-8000-00000000500e";
    agents.start({ commandId: command, agentId: id, sessionId: session, resume: false, text: "Reply with the single word: pong. Use no tools.", model: "claude-haiku-5-5", effort: "low" });
    const { home } = agents.all()[0]!;
    const models = () =>
      readFileSync(home.events(session), "utf8")
        .split("\n")
        .filter(Boolean)
        .map((l) => JSON.parse(l) as { type?: string; parent_tool_use_id?: unknown; message?: { model?: string } })
        .filter((m) => m.type === "assistant" && !m.parent_tool_use_id && m.message?.model !== "<synthetic>")
        .map((m) => m.message?.model);
    let at: Cursor = START;
    const lines: Line[] = [];
    const tokens: number[] = [];
    let usage = false;
    const readOn = () => {
      for (let b = agents.read(home, at, 1 << 20, 1000); b; b = agents.read(home, at, 1 << 20, 1000)) {
        lines.push(...b.lines);
        tokens.push(...b.tokens.map((t) => t.tokens.outputTokens));
        usage ||= Boolean(b.usage);
        at = b.next;
      }
      return agents.report(home, id, at);
    };
    try {
      await until(() => readOn().state === "waiting" || !agents.isLive(id), 150_000);
      const report = readOn();
      expect(report).toMatchObject({ state: "waiting", tasks: 0, told: true });
      expect(lines[0]).toMatchObject({ seq: 0, from: "instruction", text: "Reply with the single word: pong. Use no tools.", commandId: command });
      expect(lines.some((l) => l.from === "agent" && /pong/i.test(l.text))).toBe(true);
      expect(tokens.at(-1)).toBeGreaterThan(0);
      expect(report.contextTokens).toBeGreaterThan(0);
      expect(new Set(models())).toEqual(new Set(["claude-haiku-5-5"]));
      const answered = models().length;
      await agents.configure(id, "claude-sonnet-5-5", null);
      agents.message(id, "6a1c2f3e-0000-4000-8000-00000000c002", "Reply with the single word: ping. Use no tools.");
      await until(() => models().length > answered && readOn().state === "waiting", 150_000);
      expect(new Set(models().slice(answered))).toEqual(new Set(["claude-sonnet-5-5"]));
      console.log(`real session: models ${models().join(", ")}, context ${readOn().contextTokens} tokens`);
      console.log(`real session: ${lines.length} lines, usage ${usage ? "read" : "not said"}`);
    } finally {
      await agents.stop(id);
    }
    expect(readOn().state).toBe("idle");
  });
});
