/**
 * What this host knows of Claude Code's own shapes: the messages an Agent SDK
 * session streams (the same JSON Claude Code's stream-json says, which is
 * what each session's `logs/<id>.jsonl` keeps), what one says in lines,
 * state, tokens and usage, and the agent's own Claude Code folder.
 *
 * Every agent has a Claude Code of its own -- settings, history, projects,
 * transcripts, skills and the rules it works by, all in `CLAUDE_CONFIG_DIR`
 * under its home -- and the host user's login. HOME stays the user's: on
 * macOS the login is in the user's login Keychain, which a process with
 * another HOME does not search, and git, gh and ssh find the user's identity
 * there too. Claude Code keys its login store by `CLAUDE_CONFIG_DIR` unless
 * `CLAUDE_SECURESTORAGE_CONFIG_DIR` says where the login is, so each session
 * is given that, set to wherever the host user's own Claude Code keeps it.
 */
import { chmodSync, existsSync, mkdirSync, readdirSync, readFileSync, realpathSync, renameSync, writeFileSync } from "node:fs";
import { join, resolve, sep } from "node:path";
import { epoch } from "./sys.ts";

/** The SDK release the shapes here were checked against, by the tests and the real-Claude one. */
export const CHECKED_SDK = "0.3.323";

/**
 * What an agent says or is told is carried whole up to this many bytes, as
 * the JSON its line goes up in. A report is one socket message of 128 KB at
 * most, and may hold one such line past its own budget, so this leaves room
 * for both.
 */
export const LONGEST_TEXT = 64 << 10;

/** A tool's input is shown on one line and can be a whole file: longer than this many characters, it is cut. */
export const LONGEST_INPUT = 4096;

/** More than the note a cut text ends with can take, at its longest numbers. */
export const NOTE_ROOM = 64;

/** Tools that wait on a person at a terminal, which a session run by the host has none of: an agent asks in words and ends its turn. */
const DENIED = ["AskUserQuestion", "EnterPlanMode", "ExitPlanMode"];

/**
 * The host user's own `CLAUDE_CONFIG_DIR`, if they set one: where their login
 * is. One inside `agentsDir` is an agent's own state -- the host was started
 * from inside an agent's session -- and is passed over for Claude Code's
 * default; `warn` says so.
 */
export function loginDir(configDir: string | undefined, agentsDir: string, warn: (m: string) => void = () => {}): string | null {
  if (!configDir) return null;
  const real = (p: string) => {
    try {
      return realpathSync(p);
    } catch {
      return resolve(p);
    }
  };
  const dir = real(configDir);
  const agents = real(agentsDir);
  if (dir === agents || dir.startsWith(agents + sep)) {
    warn(`CLAUDE_CONFIG_DIR ${configDir} is an agent's own, not this user's: sessions use Claude Code's default login`);
    return null;
  }
  return configDir;
}

/** What Claude Code needs in each session's environment beside the rest: the agent's own state, and where the shared login is. Empty is Claude Code's default store. */
export function claudeEnv(state: string, login: string | null): Record<string, string> {
  return { CLAUDE_CONFIG_DIR: state, CLAUDE_SECURESTORAGE_CONFIG_DIR: login ?? "" };
}

/**
 * Readies an agent's own Claude Code state in `state` to start in `dir`. What
 * Claude Code asks a new user, or of a folder it has not seen, is answered
 * beforehand, so a person who takes the session over by hand is not asked
 * either. Answers already there are kept, so what an agent changes for
 * itself stays changed. `rules` are its standing instructions, Claude Code's
 * user memory, which it reads at the start of every session. `inside` is
 * where `dir` is to a Claude Code that runs in a container.
 */
export function prepare(state: string, dir: string, rules: string, inside?: string): void {
  mkdirSync(state, { recursive: true });
  // Claude Code keys a folder by the path it finds itself in, symlinks resolved: in a container, where it is mounted.
  const project = inside ?? realpathSync(dir);
  merge(join(state, ".claude.json"), (config) => {
    config.hasCompletedOnboarding ??= true;
    // Asked on every start once a Chrome extension is installed.
    config.hasCompletedClaudeInChromeOnboarding ??= true;
    config.claudeInChromeDefaultEnabled ??= false;
    const projects = (config.projects ??= {}) as Record<string, Record<string, unknown>>;
    if (typeof projects !== "object" || Array.isArray(projects)) throw new Error("projects is not an object");
    const entry = (projects[project] ??= {});
    if (typeof entry !== "object" || Array.isArray(entry)) throw new Error(`${project} is not an object`);
    entry.hasTrustDialogAccepted = true;
  });
  merge(join(state, "settings.json"), (settings) => {
    const permissions = (settings.permissions ??= {}) as Record<string, unknown>;
    if (typeof permissions !== "object" || Array.isArray(permissions)) throw new Error("permissions is not an object");
    const deny = (permissions.deny ??= []) as unknown[];
    if (!Array.isArray(deny)) throw new Error("permissions.deny is not an array");
    for (const tool of DENIED) if (!deny.includes(tool)) deny.push(tool);
  });
  writeFileSync(join(state, "CLAUDE.md"), rules);
  subagents(state);
}

/** Writes the host's own subagents into `state`; done before every session, a resumed one too, which `prepare` is not. */
export function subagents(state: string): void {
  mkdirSync(join(state, "agents"), { recursive: true });
  writeFileSync(join(state, "agents", "Explore.md"), EXPLORE);
}

/**
 * A user subagent named Explore takes the built-in's place, which runs on the
 * session's own model: searching needs no more than Haiku.
 */
export const EXPLORE = `---
name: Explore
description: Read-only search agent. Use to find where something is in a codebase or its docs (files, symbols, call sites, config) when you need the answer, not the file dumps. Say how broad to search.
model: haiku
disallowedTools: Write, Edit, NotebookEdit
---

You search and report; you change nothing. Find what you were asked for with
Glob, Grep and Read (and read-only shell commands such as \`git log\` or
\`ls\`), reading excerpts rather than whole files.

Answer with a compact summary of a few hundred words at most: what you found,
each finding cited as \`path:line\`, and what you looked for and did not find.
Quote only the lines that matter.
`;

/**
 * Changes the JSON object in `path`, or a new one, and writes it aside and
 * renamed over, so a machine that dies mid-write leaves the old file rather
 * than half of a new one. Readable by this user alone: Claude Code's own
 * `.claude.json` carries the account.
 */
function merge(path: string, change: (o: Record<string, unknown>) => void): void {
  let value: unknown = {};
  if (existsSync(path)) value = JSON.parse(readFileSync(path, "utf8"));
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error(`${path}: not an object`);
  const before = JSON.stringify(value);
  try {
    change(value as Record<string, unknown>);
  } catch (e) {
    throw new Error(`${path}: ${(e as Error).message}`);
  }
  if (JSON.stringify(value) === before && existsSync(path)) return;
  const aside = `${path}.agency-host`;
  writeFileSync(aside, JSON.stringify(value, null, 2), { mode: 0o600 });
  chmodSync(aside, 0o600);
  renameSync(aside, path);
}

/** Where a session's transcript is in an agent's state, once Claude Code has written it. Looked for by name: how it names a project folder is its own business. */
export function transcript(state: string, claudeSession: string): string | null {
  const projects = join(state, "projects");
  let dirs: string[];
  try {
    dirs = readdirSync(projects);
  } catch {
    return null;
  }
  for (const d of dirs) {
    const path = join(projects, d, `${claudeSession}.jsonl`);
    if (existsSync(path)) return path;
  }
  return null;
}

/**
 * Claude Code wants a uuid for a session, and for a message. A service id is
 * used as it stands when it is one, so the two can be matched by eye;
 * otherwise one is derived from it, the same every time, so a host that
 * restarts finds the session again with nothing remembered.
 */
export function uuid(id: string): string {
  if (/^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$/.test(id)) return id.toLowerCase();
  return derived(id, 4n);
}

/** The uuid of a message the host makes up itself, which carries no command: marked version 8, so its echo is told from a command's. */
export function hostUuid(id: string): string {
  return derived(id, 8n);
}

export function hostMade(u: string): boolean {
  return u[14] === "8";
}

const MASK = (1n << 64n) - 1n;

/** As the Rust host derived them, so a session it began is found under the same id. */
function derived(id: string, version: bigint): string {
  const bytes = new TextEncoder().encode(id);
  const fnv = (basis: bigint) => {
    let h = basis;
    for (const b of bytes) h = ((h ^ BigInt(b)) * 0x0000_0100_0000_01b3n) & MASK;
    return h;
  };
  let hi = fnv(0xcbf2_9ce4_8422_2325n);
  let lo = fnv(0x8422_2325_cbf2_9ce4n);
  hi = (hi & ~0xf000n & MASK) | (version << 12n);
  lo = (lo & ~(0b11n << 62n) & MASK) | (0b10n << 62n);
  const hex = (n: bigint, width: number) => n.toString(16).padStart(width, "0");
  return [
    hex(hi >> 32n, 8),
    hex((hi >> 16n) & 0xffffn, 4),
    hex(hi & 0xffffn, 4),
    hex(lo >> 48n, 4),
    hex(lo & 0xffff_ffff_ffffn, 12),
  ].join("-");
}

// --- what a message says ------------------------------------------------------

export type Entry =
  | { from: "instruction"; text: string; uuid?: string }
  | { from: "agent"; text: string }
  | { from: "tool"; text: string };

export interface Window {
  utilization: number;
  resetsAt: string;
}

export interface Windows {
  fiveHour?: Window;
  sevenDay?: Window;
}

export interface Tokens {
  inputTokens: number;
  outputTokens: number;
  cacheCreationInputTokens: number;
  cacheReadInputTokens: number;
}

/** Token counts by model id. */
export type ByModel = { [model: string]: Tokens };

export function addByModel(a: ByModel, b: ByModel): ByModel {
  const out: ByModel = { ...a };
  for (const [m, t] of Object.entries(b)) out[m] = addTokens(out[m] ?? NO_TOKENS, t);
  return out;
}

export const NO_TOKENS: Tokens = { inputTokens: 0, outputTokens: 0, cacheCreationInputTokens: 0, cacheReadInputTokens: 0 };

export function addTokens(a: Tokens, b: Tokens): Tokens {
  return {
    inputTokens: a.inputTokens + b.inputTokens,
    outputTokens: a.outputTokens + b.outputTokens,
    cacheCreationInputTokens: a.cacheCreationInputTokens + b.cacheCreationInputTokens,
    cacheReadInputTokens: a.cacheReadInputTokens + b.cacheReadInputTokens,
  };
}

/** What one message says, and when. */
export interface Read {
  at?: string;
  entries: Entry[];
  /** True when it ends a turn (a `result`), false when it shows one under way, absent when it says neither. */
  waiting?: boolean;
  /** How many background tasks and subagents the session has open after it, when it says. */
  tasks?: number;
  /** False at a turn's start, true once a message the host delivered comes back in it. */
  told?: boolean;
  /** The account's usage, when it is a `rate_limit_event` that says any. */
  usage?: Windows;
  /** When the limit resets, when it is a `rate_limit_event` that refuses the account. */
  rejected?: string;
  /**
   * What it says of the turn, for a limit: `begins` for a turn begun or a
   * command's message delivered, `continues` for the agent's own work going
   * on, `ends` for a `result`.
   */
  turn?: "begins" | "continues" | "ends";
  /** The size of the main thread's context as of this request to the model: its input, cache read and cache write tokens, when it is a main-thread assistant message that says. */
  contextTokens?: number;
  /** True for a compaction that gives no size after it: the context stands at none known until the next main-thread message says. */
  contextUnknown?: boolean;
  /** True for a main-thread request to the model, whose `contextTokens` is the size it was made with. */
  request?: boolean;
  /** True for a compaction's end and for a new process: the request before has no successor to be measured against. */
  resetsGrowth?: boolean;
  /** True once a compaction begins, false once one ends (its boundary, or a failed one): absent when it says neither. */
  compacting?: boolean;
  /** What its turn used, subagents included, when it is a `result` that says. */
  tokens?: Tokens;
  /** The same by model id, when the `result` names the models; its sum is `tokens`. */
  byModel?: ByModel;
  /** The `duration_ms` of the turn, when it is a `result` that says. */
  durationMs?: number;
  /** For the host's own record of a message it sent: that message's uuid and the command it carried. */
  sent?: { uuid: string; commandId?: string };
}

// `agent_host` records are in transcripts already written, and read back from them: the type keeps its name.

/** The record the host writes in a session's events before each Claude Code it runs for it: a new process has no background tasks. */
export function started(at: string): object {
  return { type: "agent_host", subtype: "process_started", timestamp: at };
}

/** The record the host writes before it sends a message, so the message's echo names the command it carried, whatever its uuid. */
export function sent(at: string, u: string, commandId: string | undefined): object {
  return { type: "agent_host", subtype: "sent", uuid: u, ...(commandId ? { commandId } : {}), timestamp: at };
}

type Json = { [k: string]: unknown };
const obj = (v: unknown): Json => (v && typeof v === "object" && !Array.isArray(v) ? (v as Json) : {});
const kilo = (n: number) => (n >= 1000 ? `${Math.round(n / 1000)}k` : `${n}`);
const count = (v: unknown) => (typeof v === "number" && Number.isFinite(v) && v >= 0 ? Math.floor(v) : 0);

/** A window whose numbers are missing or out of range is left out: the service refuses a whole report over one. */
function usage(v: Json): Windows | undefined {
  if (v.type !== "rate_limit_event") return undefined;
  const windows = obj(obj(v.rate_limit_info).unifiedWindows);
  const window = (name: string): Window | undefined => {
    const w = obj(windows[name]);
    if (typeof w.utilization !== "number" || !(w.utilization >= 0) || typeof w.resetsAt !== "number") return undefined;
    const resetsAt = epoch(w.resetsAt);
    return resetsAt ? { utilization: w.utilization, resetsAt } : undefined;
  };
  const read: Windows = {};
  const fiveHour = window("five_hour");
  const sevenDay = window("seven_day");
  if (fiveHour) read.fiveHour = fiveHour;
  if (sevenDay) read.sevenDay = sevenDay;
  return fiveHour || sevenDay ? read : undefined;
}

/** A `rate_limit_event` whose limit is reached: when it resets, or failing that the latest reset of the windows reached. */
function rejected(v: Json): string | undefined {
  if (v.type !== "rate_limit_event") return undefined;
  const info = obj(v.rate_limit_info);
  if (info.status !== "rejected") return undefined;
  if (typeof info.resetsAt === "number") return epoch(info.resetsAt) ?? undefined;
  const resets = Object.values(obj(info.unifiedWindows))
    .map(obj)
    .filter((w) => w.status === "rejected" && typeof w.resetsAt === "number")
    .map((w) => w.resetsAt as number);
  return resets.length ? (epoch(Math.max(...resets)) ?? undefined) : undefined;
}

/**
 * What a `result` says its turn used. Of its counts, `usage` is the main
 * conversation's alone, `modelUsage` the whole query's so far (and, resumed,
 * the transcript's before it, so it would count again what earlier sessions
 * used), and `model_usage_since_last_result` this process's since its last
 * `result`, subagents included, by model. Tokens spent after a process's last
 * `result` are in none of its results and go uncounted rather than risk
 * being counted twice.
 */
function tokens(v: Json): { total: Tokens; byModel?: ByModel } | undefined {
  if (v.type !== "result") return undefined;
  const since = v.model_usage_since_last_result;
  if (since && typeof since === "object") {
    const byModel: ByModel = {};
    for (const [model, m] of Object.entries(since as Json)) {
      const u = obj(m);
      byModel[model] = {
        inputTokens: count(u.inputTokens),
        outputTokens: count(u.outputTokens),
        cacheCreationInputTokens: count(u.cacheCreationInputTokens),
        cacheReadInputTokens: count(u.cacheReadInputTokens),
      };
    }
    return { total: Object.values(byModel).reduce(addTokens, NO_TOKENS), byModel };
  }
  // Without per-model usage the models are not named: the counts stand alone.
  if (!v.usage || typeof v.usage !== "object") return undefined;
  const u = obj(v.usage);
  return {
    total: {
      inputTokens: count(u.input_tokens),
      outputTokens: count(u.output_tokens),
      cacheCreationInputTokens: count(u.cache_creation_input_tokens),
      cacheReadInputTokens: count(u.cache_read_input_tokens),
    },
  };
}

/** The uuid of the message `v` echoes back, if it is the echo of one the host sent: Claude Code replays its own task notifications too, marked with where they came from. */
function echoed(v: Json): string | undefined {
  return v.type === "user" && v.isReplay === true && v.origin === undefined && typeof v.uuid === "string" ? v.uuid : undefined;
}

/** A line that is not JSON, or of a kind not read here (`context`: the size of the context as it stood, to say in a compaction's start), says nothing, and the lines after it are read as usual. */
export function read(line: string, context?: number): Read {
  let parsed: unknown;
  try {
    parsed = JSON.parse(line);
  } catch {
    return { entries: [] };
  }
  const v = obj(parsed);
  const out: Read = { entries: [] };
  if (typeof v.timestamp === "string") out.at = v.timestamp;
  const u = usage(v);
  if (u) out.usage = u;
  const t = tokens(v);
  if (t) {
    out.tokens = t.total;
    if (t.byModel) out.byModel = t.byModel;
  }
  if (v.type === "result") out.durationMs = count(v.duration_ms);
  const limit = rejected(v);
  if (limit) out.rejected = limit;
  if (v.type === "result") out.turn = "ends";
  if (v.type === "agent_host" && v.subtype === "sent" && typeof v.uuid === "string") {
    out.sent = { uuid: v.uuid, ...(typeof v.commandId === "string" ? { commandId: v.commandId } : {}) };
    // The host's own message to a session it resumed is not one: a limit that still holds ends the turn it begins again.
    if (out.sent.commandId) out.turn = "begins";
    // From the send the session is in a turn, though a `/compact` says nothing until it ends.
    out.waiting = false;
    return out;
  }
  // A subagent's own conversation, and lines Claude Code adds for itself.
  if (v.isSidechain === true || v.isMeta === true || typeof v.parent_tool_use_id === "string") return out;
  const message = obj(v.message);
  // Claude Code's own notices name the model `<synthetic>` and used nothing.
  if (v.type === "assistant" && message.model !== "<synthetic>" && message.usage && typeof message.usage === "object") {
    const u = obj(message.usage);
    out.contextTokens = count(u.input_tokens) + count(u.cache_read_input_tokens) + count(u.cache_creation_input_tokens);
    out.request = true;
  }
  if (v.type === "system" && v.subtype === "init") out.turn = "begins";
  // The limit's own notice is an assistant message Claude Code makes up, not the agent working on.
  else if (v.type === "user" || (v.type === "assistant" && message.model !== "<synthetic>")) out.turn = "continues";
  if (v.type === "system" && v.subtype === "background_tasks_changed" && Array.isArray(v.tasks)) out.tasks = v.tasks.length;
  // A new process has no background tasks, and is in no turn until it is told something.
  if (v.type === "agent_host" && v.subtype === "process_started") {
    out.tasks = 0;
    out.waiting = false;
    out.resetsGrowth = true;
  }
  const echo = echoed(v);
  if (v.type === "system" && v.subtype === "init") out.told = false;
  else if (echo) out.told = true;
  switch (v.type) {
    // A tool's result, too, is the agent at work.
    case "user":
      out.waiting = false;
      break;
    case "attachment":
      if (obj(v.attachment).type === "queued_command") out.waiting = false;
      break;
    // A turn begun, which may be with nothing said: a background task's end starts one.
    case "system":
      if (v.subtype === "init") out.waiting = false;
      break;
    // A turn ended, however it ended: done, failed, interrupted.
    case "result":
      out.waiting = true;
      break;
    case "assistant":
      if (typeof message.stop_reason === "string") out.waiting = message.stop_reason === "end_turn";
      break;
  }
  // The texts of one message share one allowance: a read takes a line whole,
  // so it is the line, not each text in it, that must fit a report.
  let room = LONGEST_TEXT;
  const carried = (text: string) => {
    const kept = cut(text, room, jsonLen);
    room = Math.max(0, room - [...kept].reduce((n, c) => n + jsonLen(c), 0));
    return kept;
  };
  if (v.type === "user") {
    if (typeof message.content === "string") {
      out.entries.push({ from: "instruction", text: carried(message.content), ...(echo ? { uuid: echo } : {}) });
    } else if (Array.isArray(message.content)) {
      // Tool results come back as the user's turn; they are the agent's own work, not something said to it.
      for (const b of message.content) {
        const block = obj(b);
        if (block.type === "text" && typeof block.text === "string") out.entries.push({ from: "instruction", text: carried(block.text) });
      }
    }
  } else if (v.type === "attachment" && obj(v.attachment).type === "queued_command") {
    const prompt = obj(v.attachment).prompt;
    if (typeof prompt === "string") out.entries.push({ from: "instruction", text: carried(prompt) });
  } else if (v.type === "assistant" && Array.isArray(message.content)) {
    for (const b of message.content) {
      const block = obj(b);
      if (block.type === "text" && typeof block.text === "string") out.entries.push({ from: "agent", text: carried(block.text) });
      else if (block.type === "tool_use" && typeof block.name === "string") {
        out.entries.push({ from: "tool", text: `${block.name} ${cut(JSON.stringify(block.input ?? null), LONGEST_INPUT, () => 1)}` });
      }
    }
  }
  if (v.type === "system" && v.subtype === "status") {
    if (v.status === "compacting") {
      const how = v.trigger === "auto" ? " automatically" : "";
      const size = context !== undefined ? ` (${kilo(context)} tokens of context)` : "";
      out.entries.push({ from: "agent", text: `Compacting the conversation${how}${size}…` });
      out.compacting = true;
    }
    if (v.compact_result === "failed") {
      out.entries.push({ from: "agent", text: `Compaction failed: ${typeof v.compact_error === "string" ? v.compact_error : "no reason given"}` });
      out.compacting = false;
    out.resetsGrowth = true;
    }
  }
  if (v.type === "system" && v.subtype === "compact_boundary") {
    out.compacting = false;
    const m = obj(v.compact_metadata);
    const before = typeof m.pre_tokens === "number" ? count(m.pre_tokens) : undefined;
    const after = typeof m.post_tokens === "number" ? count(m.post_tokens) : undefined;
    const how = m.trigger === "auto" ? " automatically" : "";
    const size =
      before !== undefined && after !== undefined ? `: ${kilo(before)} → ${kilo(after)} tokens of context` : before !== undefined ? ` (${kilo(before)} tokens of context)` : "";
    out.entries.push({ from: "agent", text: `Compacted the conversation${how}${size}` });
    if (after !== undefined) out.contextTokens = after;
    else out.contextUnknown = true;
  }
  return out;
}

/**
 * `text` as it is when its `size` is at most `longest`; otherwise as much of
 * it as fits with a note of how much that is. The note is the service's shape
 * for a cut text, and counts in characters (code points).
 */
export function cut(text: string, longest: number, size: (c: string) => number): string {
  const chars = [...text];
  if (chars.reduce((n, c) => n + size(c), 0) <= longest) return text;
  // An allowance shorter than the note keeps the note alone.
  const room = Math.max(0, longest - NOTE_ROOM);
  let used = 0;
  let kept = 0;
  for (const c of chars) {
    used += size(c);
    if (used > room) break;
    kept++;
  }
  return `${chars.slice(0, kept).join("")}… [cut: ${kept} of ${chars.length} characters]`;
}

/** The bytes a character takes in a JSON string, as JSON.stringify writes it. */
export function jsonLen(c: string): number {
  if (c === '"' || c === "\\" || c === "\n" || c === "\r" || c === "\t" || c === "\b" || c === "\f") return 2;
  const code = c.codePointAt(0)!;
  if (code < 0x20) return 6;
  // A lone surrogate is written escaped.
  if (code >= 0xd800 && code <= 0xdfff) return 6;
  return Buffer.byteLength(c, "utf8");
}
