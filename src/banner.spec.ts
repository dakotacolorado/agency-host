import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { banner, MARK_PIXELS, SUMMARY, TAGLINE, wantsColor, WORDMARK } from "./banner.ts";

const VERSION = (JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8")) as { version: string }).version;

describe("banner", () => {
  it("carries agency's tagline verbatim", () => {
    expect(TAGLINE).toBe("Agents at your command, across all your computers.");
    expect(SUMMARY).toBe("agency-host runs agency's agents on this computer, each a Claude Code session on the Claude Agent SDK.");
  });

  it("sets agency's mark beside the letters, a row of text for each two of pixels", () => {
    expect(MARK_PIXELS).toHaveLength(12);
    expect(new Set(MARK_PIXELS.map((row) => row.length))).toEqual(new Set([18]));
    expect(WORDMARK.split("\n")).toHaveLength(6);
    expect(WORDMARK.split("\n")[0]).toMatch(/^ █████╗ .*╗  +▄▄██▄▄$/);
    expect(WORDMARK.split("\n")[5]).toMatch(/╚═╝  +▄█{16}▄$/);
  });

  it("is the mark and letters, the version, the tagline and what this is", () => {
    expect(banner("1.2.3", false).split("\n")).toEqual([...WORDMARK.split("\n"), "v1.2.3", "", TAGLINE, SUMMARY]);
  });

  it("is painted in agency's brass only when color is on", () => {
    expect(banner("1.2.3", false)).not.toContain("\x1b");
    expect(banner("1.2.3", true)).toContain("\x1b[38;5;178m");
    expect(wantsColor({ isTTY: true }, {})).toBe(true);
    expect(wantsColor({ isTTY: true }, { NO_COLOR: "1" })).toBe(false);
    expect(wantsColor({ isTTY: false }, {})).toBe(false);
  });

  it("opens the README as agency-host prints it", () => {
    const readme = readFileSync(new URL("../README.md", import.meta.url), "utf8");
    expect(readme).toContain(`\`\`\`\n${banner(VERSION, false)}\n\`\`\`\n\n**${TAGLINE}**`);
  });
});
