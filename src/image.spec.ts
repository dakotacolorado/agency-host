import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

const arg = (file: string, name: string): string | undefined => new RegExp(`^ARG ${name}=(.*)$`, "m").exec(file)?.[1];

describe("the agent image", () => {
  it("carries the Claude Code binary the locked Agent SDK bundles", () => {
    const containerfile = readFileSync(new URL("../image/Containerfile", import.meta.url), "utf8");
    const lock = JSON.parse(readFileSync(new URL("../package-lock.json", import.meta.url), "utf8"));
    const bundled = lock.packages["node_modules/@anthropic-ai/claude-agent-sdk-linux-x64"];
    expect(arg(containerfile, "CLAUDE_SDK_VERSION")).toBe(bundled.version);
    expect(arg(containerfile, "CLAUDE_SDK_INTEGRITY")).toBe(bundled.integrity);
  });
});
