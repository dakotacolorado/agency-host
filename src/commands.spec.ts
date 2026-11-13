import { describe, expect, it } from "vitest";
import { Agents } from "./agents.ts";
import { act, Commands, parse, type Command, type CommandReport } from "./commands.ts";
import { EVERY_TOOL } from "./config.ts";
import { FakeSdk, scratch } from "./testing.ts";

function host() {
  const sdk = new FakeSdk();
  const agents = new Agents({ dir: scratch("commands"), startSession: sdk.start, permissions: EVERY_TOOL, login: null, baseEnv: {}, changed: () => {} });
  const reports: { r: CommandReport; fresh: boolean }[] = [];
  const commands = new Commands(agents, (r, fresh) => reports.push({ r, fresh }));
  return { sdk, agents, commands, reports };
}

const c = (o: object) => parse(o) as Command;

describe("a command", () => {
  it("is read field by field, or refused as not one", () => {
    expect(parse({ id: "c1", kind: "stop", agentId: "w1", later: "ignored" })).toMatchObject({ id: "c1", kind: "stop" });
    expect(parse({ kind: "stop" })).toBe("a command has no id");
    expect(parse("stop")).toBe("a command is an object");
  });

  it("of a kind the host does not handle is reported, not dropped; one without its fields says which", async () => {
    const { agents } = host();
    expect(await act(c({ id: "c1", kind: "reboot" }), agents)).toEqual({ commandId: "c1", ok: false, error: 'this host does not handle "reboot" commands' });
    for (const kind of ["create", "start", "message", "interrupt", "stop", "delete"]) {
      expect((await act(c({ id: "c1", kind, text: "hi" }), agents)).error).toBe(`"${kind}" needs an agentId`);
    }
    expect((await act(c({ id: "c1", kind: "create", agentId: "w1" }), agents)).error).toMatch(/needs a name/);
    expect((await act(c({ id: "c1", kind: "start", agentId: "w1" }), agents)).error).toMatch(/needs a sessionId/);
    expect((await act(c({ id: "c1", kind: "message", agentId: "w1" }), agents)).error).toMatch(/needs text/);
  });

  it("for an agent not here is reported; a stop or delete of one is done", async () => {
    const { agents } = host();
    for (const kind of ["start", "message", "interrupt"]) {
      const r = await act(c({ id: "c1", kind, agentId: "w9", sessionId: "s1", text: "hi" }), agents);
      expect(r.ok).toBe(false);
      expect(r.error).toMatch(/no agent w9|w9 has no live session/);
    }
    for (const kind of ["stop", "delete"]) expect(await act(c({ id: "c1", kind, agentId: "w9" }), agents)).toEqual({ commandId: "c1", ok: true });
  });

  it("message is delivered at once to a live session, and refused with none", async () => {
    const { sdk, agents } = host();
    await act(c({ id: "c1", kind: "create", agentId: "w1", name: "ana" }), agents);
    expect((await act(c({ id: "c2", kind: "message", agentId: "w1", text: "hi" }), agents)).error).toBe("agent w1 has no live session on this host");
    expect(await act(c({ id: "c3", kind: "start", agentId: "w1", sessionId: "s1", resume: false }), agents)).toEqual({ commandId: "c3", ok: true });
    expect(await act(c({ id: "c4", kind: "message", agentId: "w1", text: "hi" }), agents)).toEqual({ commandId: "c4", ok: true });
    expect(sdk.last.sent.at(-1)!.text).toBe("hi");
  });
});

describe("a command sent more than once", () => {
  it("is done once, answered again once settled, and passed over while underway", async () => {
    const { sdk, commands, reports } = host();
    commands.take(c({ id: "c1", kind: "create", agentId: "w1", name: "ana" }));
    commands.take(c({ id: "c2", kind: "start", agentId: "w1", sessionId: "s1", resume: false }));
    const message = c({ id: "c3", kind: "message", agentId: "w1", text: "once" });
    commands.take(message);
    commands.take(message);
    await commands.idle();
    commands.take(message);
    await commands.idle();
    expect(sdk.last.sent.filter((s) => s.text === "once")).toHaveLength(1);
    expect(reports).toEqual([
      { r: { commandId: "c1", ok: true }, fresh: true },
      { r: { commandId: "c2", ok: true }, fresh: true },
      { r: { commandId: "c3", ok: true }, fresh: true },
      { r: { commandId: "c3", ok: true }, fresh: false },
    ]);
  });

  it("is taken in the order it came, so a message after a start finds its session", async () => {
    const { commands, reports } = host();
    commands.take(c({ id: "c1", kind: "create", agentId: "w1", name: "ana" }));
    commands.take(c({ id: "c2", kind: "start", agentId: "w1", sessionId: "s1", resume: false }));
    commands.take(c({ id: "c3", kind: "stop", agentId: "w1" }));
    commands.take(c({ id: "c4", kind: "message", agentId: "w1", text: "late" }));
    await commands.idle();
    expect(reports.map((x) => x.r.ok)).toEqual([true, true, true, false]);
  });
});
