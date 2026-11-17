import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { zstdDecompressSync } from "node:zlib";
import { join } from "node:path";
import { describe, expect, it, vi } from "vitest";
import { Agents } from "./agents.ts";
import { archive } from "./archive.ts";
import { EVERY_TOOL } from "./config.ts";
import { FakeSdk, scratch } from "./testing.ts";

const AGENT = "3b0f6c1e-9a52-4c1d-8f43-2a6e1c7d9b10";
const S1 = "9d6a1f0e-4b7c-4e21-a3f8-0c5d2e7b4a19";

/** A .tar.zst unpacked to a plain .tar beside it: the checks' machines need not have the zstd program. */
function untar(path: string): string {
  const tar = `${path}.tar`;
  writeFileSync(tar, zstdDecompressSync(readFileSync(path)));
  return tar;
}

/** The member names of a .tar.zst, as `tar -tf` lists them. */
function members(path: string): string[] {
  return execFileSync("tar", ["-tf", untar(path)], { encoding: "utf8" }).split("\n").filter(Boolean).sort();
}

function host(o: { archiveDir?: string }) {
  const dir = scratch("agents");
  const agents = new Agents({ dir, startSession: new FakeSdk().start, permissions: EVERY_TOOL, login: null, baseEnv: { PATH: "/usr/bin" }, changed: () => {}, ...o });
  return { dir, agents };
}

/** An agent with a session, a transcript, a Claude login, a clone and a skill's own mcp.json. */
function busy(agents: Agents, dir: string) {
  agents.create(AGENT, "ana");
  agents.start({ agentId: AGENT, sessionId: S1, resume: false, text: "hi", env: { A: "1" }, mcpServers: { x: { type: "http", url: "https://x" } } });
  const folder = join(dir, "ana");
  const claude = join(folder, "home", ".claude");
  mkdirSync(join(claude, "projects", "p"), { recursive: true });
  writeFileSync(join(claude, "projects", "p", `${S1}.jsonl`), "{}\n");
  writeFileSync(join(claude, ".credentials.json"), "secret");
  mkdirSync(join(claude, "skills", "s"), { recursive: true });
  writeFileSync(join(claude, "skills", "s", "mcp.json"), "{}");
  mkdirSync(join(folder, "repo", ".git"), { recursive: true });
  writeFileSync(join(folder, "repo", "README.md"), "x");
  writeFileSync(join(folder, "notes.md"), "x");
}

describe("archiving a deleted agent", () => {
  it("writes <name>-<id>.tar.zst with its records, and none of its credentials, MCP servers or clones", async () => {
    const archiveDir = join(scratch("archive"), "agency", "agents");
    const { dir, agents } = host({ archiveDir });
    busy(agents, dir);
    await agents.delete(AGENT);
    await Promise.all(agents.archiving);
    expect(readdirSync(archiveDir)).toEqual([`ana-${AGENT}.tar.zst`]);
    const names = members(join(archiveDir, `ana-${AGENT}.tar.zst`));
    for (const kept of ["agent.json", "sessions.jsonl", "prompt.md", `logs/${S1}.jsonl`, `home/.claude/projects/p/${S1}.jsonl`, "home/.claude/CLAUDE.md"]) {
      expect(names).toContain(kept);
    }
    expect(names.filter((n) => /credentials|mcp\.json|^repo|notes\.md/.test(n))).toEqual([]);
    const out = scratch("out");
    execFileSync("tar", ["-xf", untar(join(archiveDir, `ana-${AGENT}.tar.zst`)), "-C", out]);
    expect(JSON.parse(readFileSync(join(out, "agent.json"), "utf8"))).toEqual({ id: AGENT, name: "ana" });
    expect(existsSync(join(dir, ".deleted", `ana-${AGENT}`, "agent.json"))).toBe(true);
  });

  it("makes a missing archive_dir on the local disk, mode 700, with no drive check", async () => {
    const archiveDir = join(scratch("archive"), "a", "b");
    const { dir, agents } = host({ archiveDir });
    busy(agents, dir);
    await agents.delete(AGENT);
    await Promise.all(agents.archiving);
    expect(readdirSync(archiveDir)).toEqual([`ana-${AGENT}.tar.zst`]);
    expect(statSync(archiveDir).mode & 0o777).toBe(0o700);
  });

  it("is logged, and the delete still done, when archive_dir cannot be made", async () => {
    const blocked = join(scratch("archive"), "file");
    writeFileSync(blocked, "");
    const { dir, agents } = host({ archiveDir: join(blocked, "agents") });
    busy(agents, dir);
    const err = vi.spyOn(process.stderr, "write").mockImplementation(() => true);
    try {
      await agents.delete(AGENT);
      await Promise.all(agents.archiving);
      expect(err.mock.calls.map((c) => String(c[0])).join("")).toMatch(/not archived to .*ENOTDIR/);
    } finally {
      err.mockRestore();
    }
    expect(existsSync(join(dir, ".deleted", `ana-${AGENT}`, "agent.json"))).toBe(true);
  });

  it("archives nothing when archive_dir is unset", async () => {
    const { dir, agents } = host({});
    busy(agents, dir);
    await agents.delete(AGENT);
    expect(agents.archiving.size).toBe(0);
  });

  it("leaves no partial file when tar fails", async () => {
    const out = scratch("archive");
    await expect(archive(join(out, "missing"), { id: AGENT, name: "ana" }, out)).rejects.toThrow(/tar exited/);
    expect(readdirSync(out)).toEqual([]);
  });
});
