import { readFileSync, statSync, writeFileSync, mkdirSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { claudeEnv, cut, hostMade, hostUuid, jsonLen, loginDir, LONGEST_INPUT, LONGEST_TEXT, NOTE_ROOM, prepare, read, sent, started, transcript, uuid, type Entry } from "./claude.ts";
import { LIMIT_STOP, scratch } from "./testing.ts";

const entries = (lines: object[]): Entry[] => lines.flatMap((l) => read(JSON.stringify(l)).entries);
const told = (text: string): Entry => ({ from: "instruction", text });
const said = (text: string): Entry => ({ from: "agent", text });

/** The note a cut text ends with, and the text before it. */
function note(text: string): [string, number, number] {
  const m = /^(.*)… \[cut: (\d+) of (\d+) characters\]$/s.exec(text);
  if (!m) throw new Error(`not cut: ${text.slice(0, 80)}`);
  return [m[1]!, Number(m[2]), Number(m[3])];
}

describe("reading what a session says", () => {
  it("reads what the agent was told and what it said and did", () => {
    expect(
      entries([
        { type: "user", message: { role: "user", content: "Run the tests." } },
        { type: "assistant", message: { content: [{ type: "thinking", thinking: "" }], stop_reason: null } },
        { type: "assistant", message: { content: [{ type: "text", text: "Running them." }, { type: "tool_use", name: "Bash", input: { command: "npm test" } }] } },
        { type: "user", message: { role: "user", content: [{ type: "tool_result", content: "ok" }] } },
      ]),
    ).toEqual([told("Run the tests."), said("Running them."), { from: "tool", text: 'Bash {"command":"npm test"}' }]);
  });

  it("reads a message typed while it worked as told, and passes over what was not said in this conversation", () => {
    expect(entries([{ type: "attachment", attachment: { type: "queued_command", prompt: "Also say hi." } }])).toEqual([told("Also say hi.")]);
    expect(
      entries([
        { type: "user", isSidechain: true, message: { content: "a subagent's prompt" } },
        { type: "user", isMeta: true, message: { content: "<local-command-caveat>" } },
        { type: "assistant", message: { content: [{ type: "text", text: "a subagent's step" }] }, parent_tool_use_id: "t9" },
        { type: "command_lifecycle", command_uuid: "u", state: "queued" },
        { type: "some-kind-not-yet-invented" },
      ]),
    ).toEqual([]);
    expect(read("not json {")).toEqual({ entries: [] });
  });

  it("says when a line was written, and names the message an echo carries back but not a task notification's", () => {
    const echo = read(JSON.stringify({ type: "user", message: { role: "user", content: "hi" }, uuid: "u1", isReplay: true, timestamp: "2026-11-13T08:02:33.581Z" }));
    expect(echo.at).toBe("2026-11-13T08:02:33.581Z");
    expect(echo.entries).toEqual([{ from: "instruction", text: "hi", uuid: "u1" }]);
    expect(echo.told).toBe(true);
    const notice = read(JSON.stringify({ type: "user", message: { role: "user", content: "<task-notification>" }, uuid: "u2", isReplay: true, origin: { kind: "task-notification" } }));
    expect(notice.entries).toEqual([told("<task-notification>")]);
    expect(notice.told).toBeUndefined();
    expect(read(JSON.stringify({ type: "user", message: { content: "no echo" }, uuid: "u3" })).entries).toEqual([told("no echo")]);
  });

  it("reads a turn's start and end, whatever ends it, and a turn under way", () => {
    const r = (m: object) => read(JSON.stringify(m));
    expect(r({ type: "system", subtype: "init" })).toMatchObject({ waiting: false, told: false });
    for (const ended of [{ type: "result", subtype: "success" }, { type: "result", subtype: "error_during_execution", is_error: true }]) {
      expect(r(ended).waiting).toBe(true);
    }
    for (const working of [
      { type: "user", message: { content: [{ type: "tool_result", content: "ok" }] } },
      { type: "attachment", attachment: { type: "queued_command", prompt: "x" } },
      { type: "assistant", message: { content: [], stop_reason: "tool_use" } },
    ]) {
      expect(r(working).waiting).toBe(false);
    }
    for (const neither of [{ type: "assistant", message: { content: [], stop_reason: null } }, { type: "system", subtype: "task_notification" }, { type: "rate_limit_event", rate_limit_info: {} }]) {
      expect(r(neither).waiting).toBeUndefined();
    }
    expect(r(started("2026-11-13T00:00:00.000Z"))).toMatchObject({ waiting: false, tasks: 0 });
  });

  it("counts background tasks from the list Claude Code gives, and none in a new process", () => {
    const tasks = (m: object) => read(JSON.stringify(m)).tasks;
    expect(tasks({ type: "system", subtype: "background_tasks_changed", tasks: [{ task_id: "a" }, { task_id: "b" }] })).toBe(2);
    expect(tasks({ type: "system", subtype: "background_tasks_changed", tasks: [] })).toBe(0);
    expect(tasks({ type: "system", subtype: "task_started", task_id: "a" })).toBeUndefined();
    expect(tasks(started("2026-11-13T00:00:00.000Z"))).toBe(0);
  });

  it("reads when a refused rate_limit_event's limit resets, and where a turn begins, goes on and ends", () => {
    const [event, notice, result] = LIMIT_STOP.map((m) => read(JSON.stringify(m)));
    expect(event!.rejected).toBe("2026-11-15T08:50:00.000Z");
    // Claude Code's own notice of the limit is not the agent working on.
    expect(notice!.turn).toBeUndefined();
    expect(result!.turn).toBe("ends");
    const limit = (info: object) => read(JSON.stringify({ type: "rate_limit_event", rate_limit_info: info })).rejected;
    expect(limit({ status: "rejected", unifiedWindows: { five_hour: { status: "rejected", resetsAt: 1794732600 }, seven_day: { status: "rejected", resetsAt: 1795273200 }, other: { status: "allowed", resetsAt: 1795999999 } } })).toBe(
      "2026-11-21T15:00:00.000Z",
    );
    for (const allowed of [{ status: "allowed", resetsAt: 1794732600 }, { status: "allowed_warning", resetsAt: 1794732600 }, { status: "rejected" }, {}]) expect(limit(allowed)).toBeUndefined();
    const turn = (m: object) => read(JSON.stringify(m)).turn;
    expect(turn({ type: "system", subtype: "init" })).toBe("begins");
    expect(turn(sent("2026-11-15T08:00:00.000Z", "u", "c1"))).toBe("begins");
    // The host's own message to a session it resumed leaves the limit: the turn it begins ends on it again.
    expect(turn(sent("2026-11-15T08:00:00.000Z", "u", undefined))).toBeUndefined();
    expect(turn({ type: "assistant", message: { model: "claude-opus-5-5", content: [] } })).toBe("continues");
    expect(turn({ type: "user", message: { content: [{ type: "tool_result", content: "ok" }] } })).toBe("continues");
    expect(turn({ type: "rate_limit_event", rate_limit_info: { status: "rejected" } })).toBeUndefined();
  });

  it("reads a rate_limit_event's windows, each it carries and no other", () => {
    // As a session on SDK 0.3.323 gave it.
    const recorded = {
      type: "rate_limit_event",
      rate_limit_info: {
        status: "allowed_warning",
        unifiedWindows: { five_hour: { utilization: 0.29, resetsAt: 1794573600, status: "allowed" }, seven_day: { utilization: 0.79, resetsAt: 1794668400, status: "allowed_warning" } },
      },
      timestamp: "2026-11-13T08:02:33.000Z",
    };
    expect(read(JSON.stringify(recorded)).usage).toEqual({
      fiveHour: { utilization: 0.29, resetsAt: "2026-11-13T12:40:00.000Z" },
      sevenDay: { utilization: 0.79, resetsAt: "2026-11-14T15:00:00.000Z" },
    });
    const usage = (windows: object) => read(JSON.stringify({ type: "rate_limit_event", rate_limit_info: { unifiedWindows: windows } })).usage;
    expect(usage({ five_hour: { utilization: 0, resetsAt: 1794573600 } })).toEqual({ fiveHour: { utilization: 0, resetsAt: "2026-11-13T12:40:00.000Z" } });
    expect(usage({ five_hour: { utilization: -0.1, resetsAt: 1794573600 }, seven_day: { utilization: 0.4, resetsAt: 1794668400 } })).toEqual({
      sevenDay: { utilization: 0.4, resetsAt: "2026-11-14T15:00:00.000Z" },
    });
    for (const none of [{}, { five_hour: { utilization: 0.5 } }, { seven_day: { utilization: "half", resetsAt: 1 } }, { seven_day: { utilization: 0.5, resetsAt: -5 } }]) {
      expect(usage(none)).toBeUndefined();
    }
  });

  it("reads what a result's turn used, subagents included, and not the query's running total", () => {
    // As SDK 0.3.323 gave a second turn's result: modelUsage is the query's so far.
    const second = {
      type: "result",
      usage: { input_tokens: 4, output_tokens: 77, cache_creation_input_tokens: 12172, cache_read_input_tokens: 37903 },
      modelUsage: { "claude-opus-5-5": { inputTokens: 6, outputTokens: 80, cacheReadInputTokens: 50809, cacheCreationInputTokens: 20621 } },
      model_usage_since_last_result: { "claude-opus-5-5": { inputTokens: 4, outputTokens: 77, cacheReadInputTokens: 37903, cacheCreationInputTokens: 12172 } },
    };
    expect(read(JSON.stringify(second)).tokens).toEqual({ inputTokens: 4, outputTokens: 77, cacheCreationInputTokens: 12172, cacheReadInputTokens: 37903 });
    const twoModels = {
      type: "result",
      model_usage_since_last_result: {
        haiku: { inputTokens: 1173, outputTokens: 14, cacheReadInputTokens: 0, cacheCreationInputTokens: 0 },
        opus: { inputTokens: 2, outputTokens: 3, cacheReadInputTokens: 12906, cacheCreationInputTokens: 8449 },
      },
    };
    expect(read(JSON.stringify(twoModels)).tokens).toEqual({ inputTokens: 1175, outputTokens: 17, cacheCreationInputTokens: 8449, cacheReadInputTokens: 12906 });
    expect(read(JSON.stringify({ type: "result", usage: { input_tokens: 6, output_tokens: 368 } })).tokens).toEqual({ inputTokens: 6, outputTokens: 368, cacheCreationInputTokens: 0, cacheReadInputTokens: 0 });
    expect(read(JSON.stringify({ type: "result" })).tokens).toBeUndefined();
    expect(read(JSON.stringify({ type: "assistant", message: { content: [], usage: { input_tokens: 6 } } })).tokens).toBeUndefined();
  });
});

describe("cutting", () => {
  it("carries a long reply whole, and cuts one too long to carry, saying how long it was", () => {
    const reply = "A long answer, line after line.\n".repeat(625);
    expect(entries([{ type: "assistant", message: { content: [{ type: "text", text: reply }] } }])).toEqual([said(reply)]);
    // Two bytes each, and six as JSON: the cut is by what goes up.
    for (const c of ["é", "\u0001", "🦀"]) {
      const long = c.repeat(LONGEST_TEXT);
      const [e] = entries([{ type: "assistant", message: { content: [{ type: "text", text: long }] } }]);
      expect(Buffer.byteLength(JSON.stringify(e!.text))).toBeLessThanOrEqual(LONGEST_TEXT);
      const [kept, count, total] = note(e!.text);
      expect([[...kept].length, total]).toEqual([count, LONGEST_TEXT]);
      expect(long.startsWith(kept) && count > LONGEST_TEXT / 7).toBe(true);
    }
  });

  it("makes the texts of one message share one allowance, each cut saying so", () => {
    const block = "y".repeat(60_000);
    const texts = entries([{ type: "user", message: { content: [0, 1, 2].map(() => ({ type: "text", text: block })) } }]).map((e) => e.text);
    expect(texts[0]).toBe(block);
    const size = texts.reduce((n, t) => n + Buffer.byteLength(JSON.stringify(t)), 0);
    expect(size).toBeLessThanOrEqual(LONGEST_TEXT + 2 * NOTE_ROOM);
    for (const t of texts.slice(1)) {
      const [kept, count, total] = note(t);
      expect([[...kept].length, total]).toEqual([count, 60_000]);
    }
  });

  it("cuts a long tool input and says so", () => {
    const input = { content: "x".repeat(10_000) };
    const [e] = entries([{ type: "assistant", message: { content: [{ type: "tool_use", name: "Write", input }] } }]);
    expect(e!.text.startsWith("Write {")).toBe(true);
    const shown = e!.text.slice("Write ".length);
    expect([...shown].length).toBeLessThanOrEqual(LONGEST_INPUT);
    const [kept, , total] = note(shown);
    expect(total).toBe(JSON.stringify(input).length);
    expect(JSON.stringify(input).startsWith(kept)).toBe(true);
  });

  it("counts a character's bytes as JSON writes it", () => {
    for (const c of ['"', "\\", "\n", "a", "é", "🦀", "\u0001", "\u001f"]) expect(jsonLen(c)).toBe(Buffer.byteLength(JSON.stringify(c)) - 2);
    expect(cut("short", 10, () => 1)).toBe("short");
  });
});

describe("ids", () => {
  it("takes a session's id as the service's, or derives one from it, the same every time", () => {
    expect(uuid("8550F3B6-D6FC-4F1E-A46C-3E385A545A32")).toBe("8550f3b6-d6fc-4f1e-a46c-3e385a545a32");
    const derived = uuid("agent-7");
    expect(derived).toBe(uuid("agent-7"));
    expect(derived).not.toBe(uuid("agent-8"));
    expect(uuid(derived)).toBe(derived);
    expect(derived[14]).toBe("4");
    const made = hostUuid("s1/2026-11-06T00:00:00Z");
    expect(uuid(made)).toBe(made);
    expect(hostMade(made) && !hostMade(derived)).toBe(true);
  });
});

describe("an agent's Claude Code", () => {
  it("answers every question before it is asked, keeps what the agent changed, and writes the host's rules", () => {
    const root = scratch("prepare");
    const state = join(root, "home/.claude");
    const work = join(root, "work");
    mkdirSync(state, { recursive: true });
    mkdirSync(work);
    writeFileSync(join(state, ".claude.json"), JSON.stringify({ oauthAccount: { id: "x" }, claudeInChromeDefaultEnabled: true }));
    writeFileSync(join(state, "settings.json"), JSON.stringify({ theme: "dark", permissions: { deny: ["WebFetch"] } }));
    prepare(state, work, "old");
    prepare(state, work, "the rules");
    const config = JSON.parse(readFileSync(join(state, ".claude.json"), "utf8"));
    expect(config).toMatchObject({ oauthAccount: { id: "x" }, hasCompletedOnboarding: true, claudeInChromeDefaultEnabled: true });
    expect(config.projects[work].hasTrustDialogAccepted).toBe(true);
    const settings = JSON.parse(readFileSync(join(state, "settings.json"), "utf8"));
    expect(settings).toEqual({ theme: "dark", permissions: { deny: ["WebFetch", "AskUserQuestion", "EnterPlanMode", "ExitPlanMode"] } });
    expect(readFileSync(join(state, "CLAUDE.md"), "utf8")).toBe("the rules");
    expect(statSync(join(state, ".claude.json")).mode & 0o777).toBe(0o600);
  });

  it("gives each session its own state and the user's login, unless that is an agent's", () => {
    expect(claudeEnv("/w/ana/home/.claude", null)).toEqual({ CLAUDE_CONFIG_DIR: "/w/ana/home/.claude", CLAUDE_SECURESTORAGE_CONFIG_DIR: "" });
    const agents = scratch("login");
    expect(loginDir("/u/claude", agents)).toBe("/u/claude");
    const warned: string[] = [];
    expect(loginDir(join(agents, "ana/home/.claude"), agents, (m) => warned.push(m))).toBeNull();
    expect(warned).toHaveLength(1);
    expect(loginDir("", agents)).toBeNull();
  });

  it("finds a session's transcript by its id", () => {
    const state = scratch("find");
    expect(transcript(state, "s1")).toBeNull();
    mkdirSync(join(state, "projects/-some-folder"), { recursive: true });
    writeFileSync(join(state, "projects/-some-folder/s1.jsonl"), "");
    expect(transcript(state, "s1")).toBe(join(state, "projects/-some-folder/s1.jsonl"));
  });
});
