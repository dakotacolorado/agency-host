import { describe, expect, it } from "vitest";
import { Agents } from "./agents.ts";
import { EVERY_TOOL } from "./config.ts";
import { MOST_TOKENS, REPORT_LIMIT, Reporter, Totals } from "./report.ts";
import { FakeSdk, scratch, settle } from "./testing.ts";

function host() {
  const sdk = new FakeSdk();
  const agents = new Agents({ dir: scratch("report"), startSession: sdk.start, permissions: EVERY_TOOL, login: null, baseEnv: {}, changed: () => {} });
  return { sdk, agents, reporter: new Reporter(agents) };
}

const parsed = (text: string) => JSON.parse(text) as { type: string; agents: { id: string; state: string }[]; lines: { agentId: string; seq: number; text: string }[]; commandResults: unknown[]; tokens?: unknown[]; usage?: unknown };

describe("a report", () => {
  it("lists every agent on a socket not told before, then only what changed, and nothing once all is told", async () => {
    const { sdk, agents, reporter } = host();
    agents.create("w1", "ana");
    agents.create("w2", "bob");
    const first = reporter.next(1)!;
    expect(parsed(first.text)).toEqual({ type: "report", agents: [{ id: "w1", state: "idle", sessionId: null, tasks: 0 }, { id: "w2", state: "idle", sessionId: null, tasks: 0 }], lines: [], commandResults: [] });
    first.sent();
    expect(reporter.next(1)).toBeNull();
    // A new socket is told everything again.
    reporter.next(2)!.sent();
    agents.start({ commandId: "c1", agentId: "w1", sessionId: "s1", resume: false, text: "go" });
    sdk.last.emit({ type: "rate_limit_event", rate_limit_info: { unifiedWindows: { five_hour: { utilization: 0.5, resetsAt: 1794573600 } } }, timestamp: "2026-11-13T08:00:00.000Z" });
    reporter.result({ commandId: "c1", ok: true });
    await settle();
    const r = reporter.next(2)!;
    const body = parsed(r.text);
    expect(body.agents.map((a) => a.state)).toEqual(["waiting", "idle"]);
    expect(body.lines.map((l) => [l.agentId, l.seq, l.text])).toEqual([["w1", 0, "go"], ["w1", 1, "said: go"]]);
    expect(body.commandResults).toEqual([{ commandId: "c1", ok: true }]);
    expect(body.tokens).toEqual([{ agentId: "w1", sessionId: "s1", inputTokens: 1, outputTokens: 10, cacheCreationInputTokens: 0, cacheReadInputTokens: 100 }]);
    expect(body.usage).toEqual({ readAt: "2026-11-13T08:00:00.000Z", fiveHour: { utilization: 0.5, resetsAt: "2026-11-13T12:40:00.000Z" } });
    // Not sent: the same again.
    expect(parsed(reporter.next(2)!.text)).toEqual(body);
    reporter.next(2)!.sent();
    expect(reporter.next(2)).toBeNull();
  });

  it("is held to the socket's limit, the lines read last waiting for the next", async () => {
    const { sdk, agents, reporter } = host();
    const long = "x".repeat(60 << 10);
    for (const [id, name] of [["w1", "ana"], ["w2", "bob"], ["w3", "cy"]]) {
      agents.create(id!, name!);
      sdk.setup = (s) => (s.onSend = () => {});
      agents.start({ agentId: id!, sessionId: `s-${id}`, resume: false, text: "go" });
      sdk.last.emit({ type: "assistant", message: { content: [{ type: "text", text: long }] }, parent_tool_use_id: null });
    }
    await settle();
    const seen = new Map<string, number>();
    for (let i = 0; i < 10; i++) {
      const r = reporter.next(1);
      if (!r) break;
      expect(Buffer.byteLength(r.text)).toBeLessThanOrEqual(REPORT_LIMIT);
      for (const l of parsed(r.text).lines) seen.set(`${l.agentId}/${l.seq}`, (seen.get(`${l.agentId}/${l.seq}`) ?? 0) + 1);
      r.sent();
    }
    expect([...seen.keys()].sort()).toEqual(["w1/0", "w2/0", "w3/0"]);
    expect([...seen.values()].every((n) => n === 1)).toBe(true);
  });

  it("too long for the socket puts back the lines read last, and their agents' states follow what is kept", async () => {
    const sdk = new FakeSdk();
    const agents = new Agents({ dir: scratch("fit"), startSession: sdk.start, permissions: EVERY_TOOL, login: null, baseEnv: {}, changed: () => {} });
    const reporter = new Reporter(agents, 25 << 10);
    sdk.setup = (s) => (s.onSend = () => {});
    for (const [id, name] of [["w1", "ana"], ["w2", "bob"], ["w3", "cy"]]) {
      agents.create(id!, name!);
      agents.start({ agentId: id!, sessionId: `s-${id}`, resume: false, text: "go" });
      sdk.last.emit({ type: "assistant", message: { content: [{ type: "text", text: "y".repeat(10 << 10) }] }, parent_tool_use_id: null });
      sdk.last.emit({ type: "result", subtype: "success" });
    }
    await settle();
    const first = reporter.next(1)!;
    expect(Buffer.byteLength(first.text)).toBeLessThanOrEqual(25 << 10);
    const body = parsed(first.text);
    expect(body.lines.map((l) => l.agentId)).toEqual(["w1", "w2"]);
    expect(body.agents.map((a) => a.state)).toEqual(["waiting", "waiting", "working"]);
    first.sent();
    const second = parsed(reporter.next(1)!.text);
    expect(second.lines.map((l) => l.agentId)).toEqual(["w3"]);
    expect(second.agents.map((a) => a.state)).toEqual(["waiting", "waiting", "waiting"]);
  });

  it("carries a result once, the newest for its command", () => {
    const { agents, reporter } = host();
    reporter.result({ commandId: "c1", ok: false, error: "first" });
    reporter.result({ commandId: "c1", ok: true });
    const r = reporter.next(1)!;
    expect(parsed(r.text).commandResults).toEqual([{ commandId: "c1", ok: true }]);
    reporter.result({ commandId: "c2", ok: true });
    r.sent();
    expect(parsed(reporter.next(1)!.text).commandResults).toEqual([{ commandId: "c2", ok: true }]);
    void agents;
  });

  it("carries as many results as fit, and the rest in the next", () => {
    const { reporter } = host();
    for (let i = 0; i < 20; i++) reporter.result({ commandId: `c${i}`, ok: false, error: "e".repeat(4000) });
    const first = reporter.next(1)!;
    const n = parsed(first.text).commandResults.length;
    expect(n).toBeGreaterThan(1);
    expect(n).toBeLessThan(20);
    first.sent();
    expect(parsed(reporter.next(1)!.text).commandResults).toHaveLength(Math.min(20 - n, n));
  });
});

describe("token totals", () => {
  it("are told once they change, and every one on a new socket", () => {
    const used = (n: number) => ({ inputTokens: n, outputTokens: 10 * n, cacheCreationInputTokens: 100 * n, cacheReadInputTokens: 1000 * n });
    const told = (t: { sessionId: string; inputTokens: number }[]) => t.map((x) => [x.sessionId, x.inputTokens]);
    const totals = new Totals();
    totals.add("w1", [{ sessionId: "s1", tokens: used(1) }, { sessionId: "s2", tokens: used(2) }]);
    totals.add("w2", [{ sessionId: "s3", tokens: used(3) }]);
    const first = totals.untold(1);
    expect(told(first)).toEqual([["s1", 1], ["s2", 2], ["s3", 3]]);
    totals.sent(1, first);
    expect(totals.untold(1)).toEqual([]);
    totals.add("w1", [{ sessionId: "s2", tokens: used(5) }]);
    totals.add("w2", [{ sessionId: "s3", tokens: used(3) }]);
    const changed = totals.untold(1);
    expect(told(changed)).toEqual([["s2", 5]]);
    totals.sent(1, changed);
    expect(told(totals.untold(2))).toEqual([["s1", 1], ["s2", 5], ["s3", 3]]);
    totals.keep((a) => a === "w2");
    expect(told(totals.untold(3))).toEqual([["s3", 3]]);
    for (let n = 0; n < MOST_TOKENS + 10; n++) totals.add("w2", [{ sessionId: `t${String(n).padStart(3, "0")}`, tokens: used(n) }]);
    const most = totals.untold(3);
    expect(most).toHaveLength(MOST_TOKENS);
    totals.sent(3, most);
    expect(totals.untold(3)).toHaveLength(11);
  });
});
