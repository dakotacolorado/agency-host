import { appendFileSync, mkdirSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { all, Home, deleted, validName, type Session } from "./home.ts";
import { scratch } from "./testing.ts";

const ana = { id: "w1", name: "ana" };
const session = (id: string, more: Partial<Session> = {}): Session => ({ sessionId: id, claude: `c-${id}`, from: 0, stream: true, stopped: false, ended: false, ...more });

describe("an agent's folder", () => {
  it("takes only a name that cannot reach outside the agents folder", () => {
    for (const good of ["ana", "a", "0", "front-end-2", "a".repeat(40)]) expect(validName(good)).toBe(true);
    for (const bad of ["", "-ana", "Ana", "../x", "a/b", ".deleted", "a b", "a".repeat(41)]) expect(validName(bad)).toBe(false);
  });

  it("has the layout the protocol names, and is not another agent's to take", () => {
    const dir = scratch("layout");
    const home = new Home(join(dir, "ana"));
    home.make(ana);
    expect(home.claude()).toBe(join(dir, "ana/home/.claude"));
    expect(home.events("s1")).toBe(join(dir, "ana/logs/s1.jsonl"));
    expect(JSON.parse(readFileSync(join(dir, "ana/agent.json"), "utf8"))).toEqual(ana);
    expect(all(dir).map((a) => a.record)).toEqual([ana]);
    home.make(ana);
    expect(() => home.make({ id: "w2", name: "ana" })).toThrow(/already holds agent w1/);
  });

  it("reads sessions back as written, passing over a torn line", () => {
    const dir = scratch("sessions");
    const home = new Home(join(dir, "ana"));
    home.make(ana);
    expect(home.latest()).toBeNull();
    home.begin(session("s1", { opening: "u1" }));
    home.stopped("s1");
    home.begin(session("s2"));
    appendFileSync(join(dir, "ana/sessions.jsonl"), '{"sessionId":"s3","cla');
    home.ended("s2");
    home.begin(session("s4"));
    home.withdraw("s4");
    expect(home.sessions()).toEqual([session("s1", { opening: "u1", stopped: true }), session("s2", { ended: true })]);
  });

  it("reads the sessions the Rust host wrote", () => {
    const dir = scratch("rust");
    const home = new Home(join(dir, "ana"));
    home.make(ana);
    writeFileSync(join(dir, "ana/sessions.jsonl"), '{"sessionId":"s1","claude":"c1","from":0,"stream":true,"opening":"u1"}\n{"sessionId":"s1","stopped":true}\n{"sessionId":"s0","claude":"c0","from":120}\n');
    expect(home.sessions()).toEqual([
      { sessionId: "s1", claude: "c1", from: 0, stream: true, opening: "u1", stopped: true, ended: false },
      { sessionId: "s0", claude: "c0", from: 120, stream: false, stopped: false, ended: false },
    ]);
  });

  it("is moved aside when deleted, and no longer counted", () => {
    const dir = scratch("delete");
    const home = new Home(join(dir, "ana"));
    home.make(ana);
    home.delete(ana);
    expect(all(dir)).toEqual([]);
    expect(deleted(dir, "w1")).toBe(true);
    expect(deleted(dir, "w2")).toBe(false);
    expect(readdirSync(join(dir, ".deleted"))).toEqual(["ana-w1"]);
  });

  it("is known deleted from a folder an older host set aside as retired", () => {
    const dir = scratch("retired");
    mkdirSync(join(dir, ".retired", "bo-w9"), { recursive: true });
    expect(deleted(dir, "w9")).toBe(true);
  });
});
