import { appendFileSync, chmodSync, existsSync, mkdirSync, readdirSync, readFileSync, statSync, utimesSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { all, Home, deleted, KEPT_MOST, prune, validName, type Session } from "./home.ts";
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

  it("leaves its credentials and MCP servers behind when deleted", () => {
    const dir = scratch("delete-given");
    const home = new Home(join(dir, "ana"));
    home.make(ana);
    writeFileSync(home.legacyCredentials(), "TOKEN=x\n");
    writeFileSync(home.mcp(), "{}\n");
    home.delete(ana);
    expect(readdirSync(join(dir, ".deleted", "ana-w1")).sort()).toEqual(["agent.json", "home", "logs"]);
  });

  it("is known deleted from a folder an older host set aside as retired", () => {
    const dir = scratch("retired");
    mkdirSync(join(dir, ".retired", "bo-w9"), { recursive: true });
    expect(deleted(dir, "w9")).toBe(true);
  });
});

describe("pruning deleted agents", () => {
  const DAY = 24 * 60 * 60 * 1000;
  const aside = (dir: string, where: string, name: string, ageMs: number, now: number) => {
    const path = join(dir, where, name);
    mkdirSync(join(path, "logs"), { recursive: true });
    writeFileSync(join(path, "credentials.env"), "TOKEN=x\n");
    writeFileSync(join(path, "logs", "s.jsonl"), "{}\n");
    const t = new Date(now - ageMs);
    utimesSync(path, t, t);
    return path;
  };

  it("keeps the newest 100 however old, and takes the credentials of those kept", async () => {
    const dir = scratch("prune-age");
    const now = Date.now();
    expect(KEPT_MOST).toBe(100);
    const kept = aside(dir, ".deleted", "ana-w1", 365 * DAY, now);
    const retired = aside(dir, ".retired", "cy-w3", 400 * DAY, now);
    const { removed, failed } = await prune(dir);
    expect(removed).toEqual([]);
    expect(failed).toEqual([]);
    expect(existsSync(join(kept, "logs", "s.jsonl"))).toBe(true);
    expect(existsSync(join(retired, "logs", "s.jsonl"))).toBe(true);
    expect(existsSync(join(kept, "credentials.env"))).toBe(false);
    expect(existsSync(join(retired, "credentials.env"))).toBe(false);
  });

  it("removes those past the newest kept, from .deleted and .retired, leaving each known deleted", async () => {
    const dir = scratch("prune-most");
    const now = Date.now();
    const kept = aside(dir, ".deleted", "ana-w1", DAY, now);
    const old = aside(dir, ".deleted", "bo-w2", 3 * DAY, now);
    const retired = aside(dir, ".retired", "cy-w3", 30 * DAY, now);
    const { removed, failed } = await prune(dir, 1);
    expect(removed.sort()).toEqual([old, retired].sort());
    expect(failed).toEqual([]);
    expect(statSync(old).isFile()).toBe(true);
    expect(statSync(retired).size).toBe(0);
    expect(deleted(dir, "w2")).toBe(true);
    expect(deleted(dir, "w3")).toBe(true);
    expect(existsSync(join(kept, "logs", "s.jsonl"))).toBe(true);
    expect((await prune(dir, 1)).removed).toEqual([]);
  });

  it("keeps only the newest past the most kept", async () => {
    const dir = scratch("prune-count");
    const now = Date.now();
    for (let i = 0; i < 5; i++) aside(dir, ".deleted", `a${i}-w${i}`, i * 60_000, now);
    const { removed } = await prune(dir, 3);
    expect(removed.map((p) => p.split("/").at(-1)).sort()).toEqual(["a3-w3", "a4-w4"]);
    expect(readdirSync(join(dir, ".deleted")).filter((n) => statSync(join(dir, ".deleted", n)).isDirectory()).sort()).toEqual(["a0-w0", "a1-w1", "a2-w2"]);
  });

  it("removes a folder an install left read-only", async () => {
    const dir = scratch("prune-readonly");
    const now = Date.now();
    const path = aside(dir, ".deleted", "ana-w1", 3 * DAY, now);
    mkdirSync(join(path, "cache", "mod"), { recursive: true });
    writeFileSync(join(path, "cache", "mod", "f"), "x");
    chmodSync(join(path, "cache", "mod"), 0o500);
    chmodSync(join(path, "cache"), 0o500);
    const t = new Date(now - 3 * DAY);
    utimesSync(path, t, t);
    expect((await prune(dir, 0)).removed).toEqual([path]);
    expect(statSync(path).isFile()).toBe(true);
  });

  it("is dated by the delete, not the folder's last write", () => {
    const dir = scratch("prune-dated");
    const home = new Home(join(dir, "ana"));
    home.make(ana);
    const t = new Date(Date.now() - 10 * DAY);
    utimesSync(home.dir, t, t);
    home.delete(ana);
    expect(Date.now() - statSync(join(dir, ".deleted", "ana-w1")).mtimeMs).toBeLessThan(DAY);
  });
});
