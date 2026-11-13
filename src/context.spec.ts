import { existsSync, mkdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { env, expand, readCredentials, write, writeCredentials, written, type Context } from "./context.ts";
import { scratch } from "./testing.ts";

const file = (path: string, content: string, executable = false) => ({ path, content, executable });
const ctx = (version: number, files: ReturnType<typeof file>[], key = "agent-service"): Context => ({ key, version, files });

describe("a start's files", () => {
  it("writes the context's and each document's files under skills/, scripts executable, and lists them", () => {
    const state = scratch("write");
    const done = write(state, ctx(3, [file("skills/page/SKILL.md", "How."), file("skills/page/bin/page", "#!/bin/sh", true)]), [
      ctx(2, [file("skills/approval/SKILL.md", "Approve.")], "approval"),
    ]);
    expect(readFileSync(join(state, "skills/page/SKILL.md"), "utf8")).toBe("How.");
    expect(readFileSync(join(state, "skills/approval/SKILL.md"), "utf8")).toBe("Approve.");
    expect(statSync(join(state, "skills/page/bin/page")).mode & 0o777).toBe(0o755);
    expect(statSync(join(state, "skills/page/SKILL.md")).mode & 0o777).toBe(0o644);
    expect(done).toEqual({
      key: "agent-service",
      version: 3,
      paths: ["skills/page/SKILL.md", "skills/page/bin/page"],
      documents: [{ key: "approval", version: 2, paths: ["skills/approval/SKILL.md"] }],
    });
    expect(written(state)).toEqual(done);
    expect(Object.keys(JSON.parse(readFileSync(join(state, ".context.json"), "utf8")))[0]).toBe("key");
  });

  it("takes away what the last start wrote, and nothing else", () => {
    const state = scratch("replace");
    mkdirSync(join(state, "skills/mine"), { recursive: true });
    writeFileSync(join(state, "skills/mine/SKILL.md"), "the agent's own");
    write(state, ctx(1, [file("skills/old/SKILL.md", "old"), file("skills/kept/SKILL.md", "v1")]), [ctx(1, [file("skills/doc/SKILL.md", "d")], "doc")]);
    write(state, ctx(2, [file("skills/kept/SKILL.md", "v2")]), []);
    expect(existsSync(join(state, "skills/old"))).toBe(false);
    expect(existsSync(join(state, "skills/doc"))).toBe(false);
    expect(readFileSync(join(state, "skills/kept/SKILL.md"), "utf8")).toBe("v2");
    expect(readFileSync(join(state, "skills/mine/SKILL.md"), "utf8")).toBe("the agent's own");
    expect(written(state)?.version).toBe(2);
    write(state, undefined, [ctx(1, [file("skills/doc/SKILL.md", "d")], "doc")]);
    expect(existsSync(join(state, "skills/kept"))).toBe(false);
    expect(existsSync(join(state, "skills/doc/SKILL.md"))).toBe(true);
  });

  it("refuses anything outside skills/, or a file written twice, before touching a file", () => {
    const state = scratch("refuse");
    write(state, ctx(1, [file("skills/a/SKILL.md", "a")]), []);
    for (const bad of ["CLAUDE.md", "settings.json", "skills", "skills/../settings.json", "/etc/passwd", "skills/./a", ".context.json", ""]) {
      expect(() => write(state, ctx(2, [file("skills/b/SKILL.md", "b"), file(bad, "x")]), [])).toThrow(/may write only/);
    }
    expect(() => write(state, ctx(2, [file("skills/x/SKILL.md", "1")]), [ctx(1, [file("skills/x/SKILL.md", "2")], "approval")])).toThrow(
      "approval writes skills/x/SKILL.md, as one before it does",
    );
    expect(existsSync(join(state, "skills/a/SKILL.md"))).toBe(true);
    expect(existsSync(join(state, "skills/b"))).toBe(false);
  });
});

describe("a start's variables", () => {
  it("go to a file only its owner reads, read back as given, and away with none", () => {
    const dir = scratch("credentials");
    const path = join(dir, "credentials.env");
    writeFileSync(path, "OLD=1", { mode: 0o644 });
    writeCredentials(path, { WT_TOKEN: "it's $x `y`\nz", DS_URL: "https://ds" });
    expect(readCredentials(path)).toEqual({ WT_TOKEN: "it's $x `y`\nz", DS_URL: "https://ds" });
    expect(readFileSync(path, "utf8")).toContain("WT_TOKEN='it'\\''s $x `y`");
    expect(statSync(path).mode & 0o777).toBe(0o600);
    expect(() => writeCredentials(path, { "A-B": "x" })).toThrow(/not a variable/);
    writeCredentials(path, {});
    expect(existsSync(path)).toBe(false);
    writeCredentials(path, {});
  });

  it("never set what the host sets itself", () => {
    expect(env({ DS_URL: "https://ds", CLAUDE_CONFIG_DIR: "/elsewhere", CLAUDE_SECURESTORAGE_CONFIG_DIR: "/x" })).toEqual({ DS_URL: "https://ds" });
    expect(env(undefined)).toEqual({});
  });

  it("fill an MCP server's ${VAR}s, and leave one with no value as written", () => {
    const servers = {
      tracker: { type: "http", url: "${WT_URL}/mcp", headers: { Authorization: "Bearer ${WT_TOKEN}" } },
      other: { type: "http", url: "${NOPE}/mcp", headers: { X: "${MAYBE:-none}" } },
    };
    expect(expand(servers, { WT_URL: "https://wt", WT_TOKEN: "s3cret" })).toEqual({
      servers: {
        tracker: { type: "http", url: "https://wt/mcp", headers: { Authorization: "Bearer s3cret" } },
        other: { type: "http", url: "${NOPE}/mcp", headers: { X: "none" } },
      },
      missing: ["NOPE"],
    });
  });
});
