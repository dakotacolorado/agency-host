/**
 * The agents on this host. Each has a folder of its own (home.ts), and while
 * it has a session, an Agent SDK session (sdk.ts), which takes messages while
 * it works and says what it does as it goes. Every message a session gives
 * is kept in its events file before anything reads it, and lines, state,
 * tokens and usage are read from those files alone, so a host that restarts
 * and reads them again from the start comes to the same.
 */
import { appendFileSync, closeSync, existsSync, mkdirSync, openSync, readFileSync, readSync, statSync, unlinkSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";
import * as claude from "./claude.ts";
import { archive } from "./archive.ts";
import { allows, type Permissions } from "./config.ts";
import * as container from "./container.ts";
import * as context from "./context.ts";
import * as home from "./home.ts";
import type { Home, Record as AgentRecord, Session } from "./home.ts";
import type { Effort, Message, SessionHandle, StartSession } from "./sdk.ts";
import { errorText, isInstant, log, now } from "./sys.ts";

/** Claude Code does nothing until it is told something, so a session always opens with words. */
export const NOTHING_YET = "Nothing has been asked of you yet. Wait to be told what the work is.";

export const CARRY_ON =
  "Your last session ended and this one carries on from it. If you were waiting to be told something, keep " +
  "waiting; otherwise carry on where you left off.";

export const RESUMED =
  "The host your session runs on restarted, and this session carries on the conversation from before it. " +
  "Whatever was under way as it went -- a reply, a tool -- was cut off and may not have finished. If you were " +
  "waiting to be told something, keep waiting; otherwise check what was cut off, then carry on where you left off.";

export const DRAINED =
  "The host your session runs on restarted for a new release, between your turns: nothing was cut off. If you " +
  "were waiting to be told something, keep waiting; otherwise carry on where you left off.";

/** However much has piled up, a read stops after this many bytes of events, most of which is Claude Code's own bookkeeping. */
const RAW_LIMIT = 8 << 20;
const CHUNK = 256 << 10;

/** The most tasks the service takes for one agent; more refuses the report. */
const MOST_TASKS = 1000;

/** How long a stopped session has to end before the host goes on without it. */
const GRACE_MS = 5000;

export type State = "idle" | "working" | "waiting" | "interrupted";

export interface AgentReport {
  id: string;
  state: State;
  sessionId: string | null;
  tasks: number;
  told?: boolean;
  /** When the usage limit that ended its last turn resets; null once a turn has begun since, or when none did. */
  limitedUntil: string | null;
  /** Results its live session has ended, and the sum of their `duration_ms`. */
  turns?: number;
  activeMs?: number;
  /** The size of its session's context as of its last request to the model, once it has made one. */
  contextTokens?: number;
  /** Held but not run, for want of the secrets a start gives; the service answers with a start that resumes it. */
  needsStart?: boolean;
}

export interface Line {
  seq: number;
  at: string;
  sessionId: string;
  from: "agent" | "tool" | "instruction";
  text: string;
  commandId?: string;
}

export interface Usage extends claude.Windows {
  readAt: string;
}

/**
 * How far into an agent's events has been accounted for: which session's
 * file, the byte after the last whole line read in it, the `seq` of the line
 * that comes next, and as of that line: whether its turn has ended, how many
 * background tasks are open, whether its turn was begun by a message, the
 * tokens its session has used, the messages sent and not yet echoed, when
 * the limit that ended its last turn resets, and when a limit reached in the
 * turn under way does.
 * Never changed in place, so a read can be put back.
 */
export interface Cursor {
  file: number;
  offset: number;
  seq: number;
  waiting: boolean;
  tasks: number;
  told?: boolean;
  tokens: claude.Tokens;
  /** Of the session's turns: the tokens by model, the results read and the sum of their durations. */
  byModel?: claude.ByModel;
  turns?: number;
  activeMs?: number;
  pending: { [uuid: string]: string | null };
  limitedUntil?: string;
  rejected?: string;
  contextTokens?: number;
}

export const START: Cursor = { file: 0, offset: 0, seq: 0, waiting: false, tasks: 0, tokens: claude.NO_TOKENS, pending: {} };

export interface Batch {
  lines: Line[];
  next: Cursor;
  usage?: Usage;
  /** Each session whose tokens the lines read added to, with its total as of the last of them. */
  tokens: { sessionId: string; tokens: claude.Tokens; byModel?: claude.ByModel }[];
}

/** What a start carries. */
export interface Start {
  commandId?: string;
  agentId: string;
  sessionId: string;
  resume: boolean;
  text?: string;
  context?: context.Context;
  documents?: context.Context[];
  env?: { [k: string]: string };
  secrets?: { [k: string]: string };
  mcpServers?: { [k: string]: unknown };
  model?: string;
  effort?: Effort;
  /** Where it runs; absent, as the agent's last session did, or on the host. */
  runtime?: "container" | "host";
}

interface Live {
  home: Home;
  name: string;
  sessionId: string;
  handle: SessionHandle;
  stopping: boolean;
  done: Promise<void>;
  /** As of the last message it gave, as a read's cursor has them: whether its turn has ended, and its background tasks open. */
  waiting: boolean;
  tasks: number;
  /** The messages sent to it that it has not yet taken, oldest first. */
  unechoed: { uuid: string; text: string }[];
}

export interface AgentsOptions {
  dir: string;
  startSession: StartSession;
  permissions: Permissions;
  /** The host user's Claude login (see `claude.loginDir`). */
  login: string | null;
  /** The environment every session starts from, before its own variables. */
  baseEnv: { [k: string]: string };
  /** Called whenever something an agent report or line could say has changed. */
  changed: () => void;
  /** Where a deleted agent's records are archived (`archive.ts`); unset, they are not. */
  archiveDir?: string;
  /** How this host runs a session in a container; unset, it runs none, and refuses a start that asks for one. */
  containers?: container.Containers;
}

/**
 * The agent's standing rules, Claude Code's user memory: the host's own, then
 * `instructions`, the honed skills its last start gave it (`context.instructions`).
 */
export function rules(dir: string, name: string, instructions = ""): string {
  const own =
    `You are agent \`${name}\`, and your folder is \`${dir}\`. A person reads everything you write, and what to do ` +
    "comes to you as messages. Clone whatever you need into your folder; `home/`, `logs/`, `env.json`, " +
    "`mcp.json`, `prompt.md`, `sessions.jsonl` and `agent.json` are the host's records of you, so leave them be.\n";
  if (!instructions) return own;
  return (
    `${own}\nYou work by the honed skills you were started with, below, in the order you were given them. Follow them as these ` +
    "rules; where one was wrong or missing something your work needed, say so as feedback on it in honed.\n\n" +
    instructions
  );
}

export class Agents {
  private readonly o: AgentsOptions;
  private readonly live = new Map<string, Live>();
  /**
   * The secrets of each live agent's latest start, by agent id: in memory only, never written. A session the
   * host resumes by itself after it restarts has none until the next start gives it some.
   */
  private readonly secrets = new Map<string, Record<string, string>>();
  /** Agents whose held session was not run, for want of secrets: reported as needing a start until one comes. */
  private readonly awaiting = new Set<string>();
  private pruning: Promise<void> | null = null;
  /** Archives under way, so a test can wait them out. */
  readonly archiving = new Set<Promise<void>>();
  /** Set once a drain has begun: done when no session is live. */
  private draining: { done: Promise<void>; resolve: () => void } | null = null;

  constructor(options: AgentsOptions) {
    this.o = options;
  }

  all(): { home: Home; record: AgentRecord }[] {
    return home.all(this.o.dir);
  }

  private find(id: string): { home: Home; record: AgentRecord } {
    const found = this.all().find((a) => a.record.id === id);
    if (found) return found;
    throw new Error(home.deleted(this.o.dir, id) ? `agent ${id} is deleted` : `no agent ${id} on this host`);
  }

  /** The live session of each agent that has one, by agent id. */
  sessions(): Map<string, string> {
    return new Map([...this.live].map(([id, l]) => [id, l.sessionId]));
  }

  isLive(id: string): boolean {
    return this.live.has(id);
  }

  /** Makes the agent's folder: its record and a Claude Code state of its own. One the host has made already is taken up where it stands. */
  create(id: string, name: string): void {
    if (!home.validName(name)) throw new Error(`${JSON.stringify(name)} is not an agent name: [a-z0-9][a-z0-9-]{0,39}`);
    if (home.deleted(this.o.dir, id)) throw new Error(`agent ${id} was deleted here`);
    const held = this.all().find((a) => a.record.id === id);
    if (held && held.record.name !== name) throw new Error(`agent ${id} is here already, named ${held.record.name}`);
    const h = new home.Home(`${this.o.dir}/${name}`);
    h.make({ id, name });
    claude.prepare(h.claude(), h.dir, rules(h.dir, name, context.instructions(h.claude())));
  }

  /**
   * Runs a session for the agent in its folder: a new conversation, or with
   * `resume` the agent's last one carried on. A start sent again that the
   * host has done already -- before a restart that lost its report -- is
   * done, not repeated: its session is running, or it ran and has ended.
   */
  start(s: Start): void {
    const { home: h, record } = this.find(s.agentId);
    const running = this.live.get(s.agentId);
    if (running) {
      if (running.sessionId === s.sessionId) return;
      throw new Error(`${record.name} already has session ${running.sessionId} running: stop it first`);
    }
    const sessions = h.sessions();
    if (sessions.some((x) => x.sessionId === s.sessionId)) return;
    const previous = s.resume ? sessions.at(-1) : undefined;
    const boxed = (s.runtime ?? (previous?.runtime === "container" ? "container" : "host")) === "container";
    if (boxed && !this.o.containers) throw new Error(`${record.name} runs in a container, and this host has no agent image to run one in`);
    const opening = s.text !== undefined && s.commandId ? claude.uuid(s.commandId) : undefined;
    const session: Session = {
      sessionId: s.sessionId,
      claude: previous?.claude ?? claude.uuid(s.sessionId),
      from: 0,
      stream: true,
      ...(opening ? { opening } : {}),
      // Only what this start names: one that names none runs on Claude Code's defaults, as before there were any.
      ...(s.model ? { model: s.model } : {}),
      ...(s.effort ? { effort: s.effort } : {}),
      ...(boxed ? { runtime: "container" as const } : {}),
      stopped: false,
      ended: false,
    };
    const text = s.text ?? (previous ? CARRY_ON : NOTHING_YET);
    writeFileSync(h.prompt(), text);
    // A start that carries neither keeps what the last one wrote.
    if (s.context !== undefined || s.documents !== undefined) context.write(h.claude(), s.context, s.documents ?? []);
    // The rules follow the host's version, so each session is told the ones of the host it runs on,
    // with the honed skills it was last given, and its folder where its Claude Code sees it.
    const seen = boxed ? container.INSIDE : h.dir;
    claude.prepare(h.claude(), h.dir, rules(seen, record.name, context.instructions(h.claude())), boxed ? container.INSIDE : undefined);
    // Its secrets stay in memory and go only to the session's environment; the addresses, which are no secret,
    // are kept so a session resumed after the host restarts has them again.
    this.secrets.set(s.agentId, context.env(s.secrets));
    this.awaiting.delete(s.agentId);
    writeFileSync(h.env(), `${JSON.stringify(context.env(s.env), null, 2)}\n`);
    this.forgetLegacy(h);
    // A start names every server its session may call, so one that names none takes away the last start's.
    if (s.mcpServers && Object.keys(s.mcpServers).length) {
      writeFileSync(h.mcp(), `${JSON.stringify({ mcpServers: s.mcpServers }, null, 2)}\n`);
    } else if (existsSync(h.mcp())) {
      unlinkSync(h.mcp());
    }
    // Recorded before it runs, so a machine that dies between the two leaves a session it knows of, which is then resumed.
    h.begin(session);
    try {
      this.run(h, record, session, previous ? { resume: session.claude } : { fresh: session.claude }, text, opening ?? this.madeUp(session), opening ? s.commandId : undefined);
    } catch (e) {
      h.withdraw(s.sessionId);
      throw e;
    }
  }

  /** A credentials.env an older host left holds tokens it no longer reads: removed. */
  private forgetLegacy(h: Home): void {
    if (existsSync(h.legacyCredentials())) unlinkSync(h.legacyCredentials());
  }

  /** What a session of the agent is given: its addresses, then its secrets. */
  private given(h: Home, agentId: string): Record<string, string> {
    let addresses: Record<string, string> = {};
    try {
      addresses = context.env(JSON.parse(readFileSync(h.env(), "utf8")) as Record<string, string>);
    } catch {
      // Never started by a host that keeps them here.
    }
    return { ...addresses, ...this.secrets.get(agentId) };
  }

  private madeUp(session: Session): string {
    return claude.hostUuid(`${session.sessionId}/${now()}`);
  }

  /** Runs Claude Code for `session` and tells it `text` under `uuid`, the message carrying `commandId` when it is a command's. */
  private run(h: Home, record: AgentRecord, session: Session, begin: { fresh: string } | { resume: string }, text: string, uuid: string, commandId: string | undefined): void {
    const events = h.events(session.sessionId);
    mkdirSync(dirname(events), { recursive: true });
    this.append(events, claude.started(now()));
    claude.subagents(h.claude());
    const boxed = session.runtime === "container" ? this.o.containers : undefined;
    if (session.runtime === "container" && !boxed) throw new Error(`${record.name} runs in a container, and this host has no agent image to run one in`);
    // In a container, nothing of the host's environment: its own variables, and its home and Claude Code where it sees them.
    const env: Record<string, string> = boxed
      ? { ...this.given(h, record.id), HOME: container.INSIDE_HOME, CLAUDE_CONFIG_DIR: container.INSIDE_STATE }
      : { ...this.o.baseEnv, ...this.given(h, record.id), ...claude.claudeEnv(h.claude(), this.o.login) };
    let mcpServers: Record<string, unknown> | undefined;
    if (existsSync(h.mcp())) {
      const given = (JSON.parse(readFileSync(h.mcp(), "utf8")) as { mcpServers?: Record<string, unknown> }).mcpServers ?? {};
      const { servers, missing } = context.expand(given, env);
      if (missing.length) log(`agent ${record.name}: its MCP servers name ${missing.join(", ")}, which its session is not given`);
      // A container's Claude Code fills the ${VAR}s itself from its environment: the SDK passes the servers on its
      // command line, which a token must not be on.
      mcpServers = boxed ? given : servers;
    }
    const errors = h.errors(session.sessionId);
    const handle = this.o.startSession({
      cwd: boxed ? container.INSIDE : h.dir,
      env,
      ...(boxed ? { container: { ...boxed, name: `agent-${record.id}`, dir: h.dir, limits: container.limits(h.claude()) } } : {}),
      ...("resume" in begin ? { resume: begin.resume } : { sessionId: begin.fresh }),
      ...(mcpServers ? { mcpServers } : {}),
      ...(session.model ? { model: session.model } : {}),
      ...(session.effort ? { effort: session.effort as Effort } : {}),
      permit: (tool) => (allows(this.o.permissions, tool) ? true : `${tool} is not allowed on this host (agency-host's permissions)`),
      stderr: (t) => {
        try {
          appendFileSync(errors, t);
        } catch {
          // Its stderr is for a person reading after; losing some loses nothing the host needs.
        }
      },
    });
    const live: Live = { home: h, name: record.name, sessionId: session.sessionId, handle, stopping: false, done: Promise.resolve(), waiting: false, tasks: 0, unechoed: [] };
    this.live.set(record.id, live);
    live.done = this.pump(h, record, live, events);
    this.say(live, events, text, uuid, commandId);
    this.o.changed();
  }

  /** Keeps every message the session gives, until it ends; one that ends without a stop is noted as ended, so the agent reads as interrupted and a host that restarts leaves it. */
  private async pump(h: Home, record: AgentRecord, live: Live, events: string): Promise<void> {
    let why = "its stream ended";
    try {
      for await (const m of live.handle.events) {
        this.append(events, stamped(m));
        follow(live, m);
        this.o.changed();
        this.closeIfBetweenTurns(live);
      }
    } catch (e) {
      why = errorText(e);
    } finally {
      if (this.live.get(record.id) === live) this.live.delete(record.id);
      if (!live.stopping) {
        log(`agent ${record.name}: session ${live.sessionId} ended by itself (${why})`);
        const latest = h.latest();
        if (latest?.sessionId === live.sessionId && !latest.stopped && existsSync(h.dir)) h.ended(live.sessionId);
      }
      this.o.changed();
      this.drainedOut();
    }
  }

  private say(live: Live, events: string, text: string, uuid: string, commandId: string | undefined): void {
    this.append(events, claude.sent(now(), uuid, commandId));
    live.unechoed.push({ uuid, text });
    live.handle.send(text, uuid);
  }

  /** A message on a line of its own, even after one a machine that died left torn. */
  private append(events: string, message: object): void {
    let line = `${JSON.stringify(message)}\n`;
    try {
      const size = statSync(events).size;
      if (size) {
        const fd = openSync(events, "r");
        const last = Buffer.alloc(1);
        readSync(fd, last, 0, 1, size - 1);
        closeSync(fd);
        if (last[0] !== 0x0a) line = `\n${line}`;
      }
    } catch {
      // Not there yet.
    }
    appendFileSync(events, line);
  }

  /**
   * Pushes `text` into the agent's live session now, whether it is working or
   * waiting: working, it takes it at its next step. One a drain has closed is
   * resumed to take it, and closed again as that turn ends.
   */
  message(id: string, commandId: string, text: string): void {
    const found = this.live.has(id) ? undefined : this.all().find((a) => a.record.id === id);
    const latest = found?.home.latest();
    if (found && latest?.drained && !latest.stopped && !latest.ended) {
      if (!this.secrets.has(id)) {
        this.awaiting.add(id);
        throw new Error(`agent ${id} waits for a start with its secrets: send the message again once it has one`);
      }
      found.home.drained(latest.sessionId, false);
      this.run(found.home, found.record, latest, { resume: latest.claude }, text, claude.uuid(commandId), commandId);
      return;
    }
    const live = this.running(id);
    this.say(live, this.find(id).home.events(live.sessionId), text, claude.uuid(commandId), commandId);
  }

  /** Stops the turn the agent is in, as Escape does; the session goes on, waiting, and `text` is what it is told next. */
  async interrupt(id: string, commandId: string, text: string | undefined): Promise<void> {
    const live = this.running(id);
    await live.handle.interrupt();
    if (text !== undefined) this.say(live, this.find(id).home.events(live.sessionId), text, claude.uuid(commandId), commandId);
  }

  /**
   * Switches the agent's session to `model` and `effort` from its next
   * request to the model, and records it, so the session resumes on them. A
   * session a drain closed, to be resumed by the next host, is recorded alone.
   * With none to switch, the next start names the model.
   */
  async configure(id: string, model: string, effort: Effort | null): Promise<void> {
    const { home: h } = this.find(id);
    const live = this.live.get(id);
    if (live) {
      await live.handle.configure(model, effort);
      h.configured(live.sessionId, model, effort);
      return;
    }
    const latest = h.latest();
    if (latest?.drained && !latest.stopped && !latest.ended) {
      h.configured(latest.sessionId, model, effort);
      return;
    }
    throw new Error(`agent ${id} has no live session on this host: its next start names the model`);
  }

  private running(id: string): Live {
    const live = this.live.get(id);
    if (!live) throw new Error(`agent ${id} has no live session on this host`);
    return live;
  }

  /** Ends the agent's session; the agent is idle. One with no session, or not here at all, is already stopped. */
  async stop(id: string): Promise<void> {
    let found;
    try {
      found = this.find(id);
    } catch {
      return;
    }
    const live = this.live.get(id);
    if (live) {
      live.stopping = true;
      live.handle.close();
      await Promise.race([live.done, new Promise((r) => setTimeout(r, GRACE_MS).unref())]);
      if (this.live.get(id) === live) this.live.delete(id);
    }
    const latest = found.home.latest();
    if (latest && !latest.stopped) found.home.stopped(latest.sessionId);
    this.o.changed();
    this.drainedOut();
  }

  /**
   * Stops the agent and moves its folder aside, without its credentials, until a prune removes it, and
   * archives its records. One not here, or deleted already, is done.
   */
  async delete(id: string): Promise<void> {
    let found;
    try {
      found = this.find(id);
    } catch {
      return;
    }
    await this.stop(id);
    const aside = found.home.delete(found.record);
    this.o.changed();
    if (this.o.archiveDir) this.archive(aside, found.record, this.o.archiveDir);
    void this.prune();
  }

  /**
   * In the background: a disk that hangs must not hold up the delete, and one that fails never fails it,
   * since the folder in `.deleted` still holds everything.
   */
  private archive(aside: string, record: AgentRecord, dir: string): void {
    const label = `${record.name}-${record.id}`;
    const p: Promise<void> = archive(aside, record, dir)
      .then((path) => log(`${label}: archived to ${path}`))
      .catch((e) => log(`${label}: not archived to ${dir}: ${errorText(e)}`))
      .finally(() => this.archiving.delete(p));
    this.archiving.add(p);
  }

  /** Removes deleted agents' folders past what is kept (`home.prune`); one prune at a time, which a call during it shares. */
  prune(): Promise<void> {
    this.pruning ??= home
      .prune(this.o.dir)
      .then(({ removed, failed }) => {
        if (removed.length) log(`removed ${removed.length} deleted agents' folders`);
        for (const f of failed) log(`${f}: not removed, tried again at the next prune`);
      })
      .catch((e) => log(`pruning deleted agents: ${errorText(e)}`))
      .finally(() => {
        this.pruning = null;
      });
    return this.pruning;
  }

  /** Stops every session, as the host goes. */
  async closeAll(): Promise<void> {
    await Promise.all(
      [...this.live.values()].map((l) => {
        // Not a stop: each is resumed when the host is back.
        l.stopping = true;
        l.handle.close();
        return Promise.race([l.done, new Promise((r) => setTimeout(r, GRACE_MS).unref())]);
      }),
    );
  }

  /**
   * Closes each live session at the end of its turn, as `closeAll` does, so
   * none loses one: a session between turns now, and each other as its turn
   * ends, a session started or told something meanwhile too. Each is marked
   * drained before it is closed, so the host that takes it up tells it it
   * lost nothing. Done once no session is live; called again, the same.
   */
  drain(): Promise<void> {
    if (!this.draining) {
      let resolve!: () => void;
      const done = new Promise<void>((r) => (resolve = r));
      this.draining = { done, resolve };
      for (const l of this.live.values()) this.closeIfBetweenTurns(l);
      this.drainedOut();
    }
    return this.draining.done;
  }

  /**
   * Between turns is as the host reports it waiting rather than working, with
   * no background task open, which would begin a turn of its own as it ends,
   * and nothing sent to it still to come back, which it has yet to take.
   */
  private closeIfBetweenTurns(live: Live): void {
    if (!this.draining || live.stopping || !live.waiting || live.tasks > 0 || live.unechoed.length) return;
    live.home.drained(live.sessionId);
    // Not a stop: each is resumed when the host is back.
    live.stopping = true;
    log(`agent ${live.name}: session ${live.sessionId} closed between turns, to be resumed by the next host`);
    live.handle.close();
  }

  private drainedOut(): void {
    if (this.draining && !this.live.size) this.draining.resolve();
  }

  /**
   * Takes up every session that was live when the host last went: each is
   * resumed and told the host restarted, losing the turn that was in flight,
   * or, one a drain closed between turns, that it lost nothing.
   * One cut off before Claude Code wrote anything has nothing to resume, and
   * is begun again as it was first. One that cannot be begun is noted as
   * ended, and reads as interrupted.
   */
  reconcile(): void {
    for (const { home: h, record } of this.all()) {
      const latest = h.latest();
      this.forgetLegacy(h);
      if (!latest || latest.stopped || latest.ended || this.live.has(record.id)) continue;
      // A session is never run without the secrets a start gives it, and a restarted host holds none: it is
      // reported as needing a start, which resumes it.
      if (!this.secrets.has(record.id)) {
        this.awaiting.add(record.id);
        log(`agent ${record.name}: session ${latest.sessionId} waits for a start with its secrets`);
        continue;
      }
      try {
        const session = latest.stream ? latest : { ...latest, from: 0, stream: true };
        if (!latest.stream) h.begin(session);
        if (claude.transcript(h.claude(), session.claude)) {
          // Cleared first, so one cut off in the session it is resumed into is told so when the host is next back.
          if (latest.drained) h.drained(latest.sessionId, false);
          this.run(h, record, session, { resume: session.claude }, latest.drained ? DRAINED : RESUMED, this.madeUp(session), undefined);
        } else {
          let text = NOTHING_YET;
          try {
            text = readFileSync(h.prompt(), "utf8");
          } catch {
            // The prompt is written before the session is recorded; without it, it opens as an empty start does.
          }
          const commandId = session.opening ? this.openingCommand(h, session) : undefined;
          this.run(h, record, session, { fresh: session.claude }, text, session.opening ?? this.madeUp(session), commandId);
        }
        log(`agent ${record.name}: session ${latest.sessionId} resumed`);
      } catch (e) {
        log(`agent ${record.name}: session ${latest.sessionId} could not be resumed: ${errorText(e)}`);
        h.ended(latest.sessionId);
      }
    }
  }

  /** The command a session's opening message carried, as the host recorded it when it first sent it. */
  private openingCommand(h: Home, session: Session): string | undefined {
    try {
      for (const line of readFileSync(h.events(session.sessionId), "utf8").split("\n")) {
        const r = claude.read(line);
        if (r.sent && r.sent.uuid === session.opening) return r.sent.commandId;
      }
    } catch {
      // No events: the host went before it sent it.
    }
    return claude.hostMade(session.opening!) ? undefined : session.opening;
  }

  /** What is reported of the agent, as of `at` in its events. */
  report(h: Home, id: string, at: Cursor): AgentReport {
    const live = this.live.get(id);
    const limitedUntil = at.limitedUntil ?? null;
    if (live) {
      return {
        id,
        state: at.waiting ? "waiting" : "working",
        sessionId: live.sessionId,
        tasks: Math.min(at.tasks, MOST_TASKS),
        ...(at.told !== undefined ? { told: at.told } : {}),
        limitedUntil,
        turns: at.turns ?? 0,
        activeMs: at.activeMs ?? 0,
        ...(at.contextTokens !== undefined ? { contextTokens: at.contextTokens } : {}),
      };
    }
    const latest = h.latest();
    if (this.awaiting.has(id) && latest && !latest.stopped && !latest.ended) {
      return {
        id,
        state: "waiting",
        sessionId: latest.sessionId,
        tasks: 0,
        needsStart: true,
        ...(at.told !== undefined ? { told: at.told } : {}),
        limitedUntil,
        ...(at.contextTokens !== undefined ? { contextTokens: at.contextTokens } : {}),
      };
    }
    // Closed by a drain between turns: it waits, for the next host to resume it.
    if (latest?.drained && !latest.stopped && !latest.ended) {
      return {
        id,
        state: "waiting",
        sessionId: latest.sessionId,
        tasks: 0,
        ...(at.told !== undefined ? { told: at.told } : {}),
        limitedUntil,
        ...(at.contextTokens !== undefined ? { contextTokens: at.contextTokens } : {}),
      };
    }
    const interrupted = latest && !latest.stopped;
    return {
      id,
      state: interrupted ? "interrupted" : "idle",
      sessionId: interrupted ? latest.sessionId : null,
      // A session not running has no tasks, whatever it last said.
      tasks: 0,
      ...(at.told !== undefined ? { told: at.told } : {}),
      // A stopped agent is not to be resumed when its limit resets.
      limitedUntil: interrupted ? limitedUntil : null,
      ...(at.contextTokens !== undefined ? { contextTokens: at.contextTokens } : {}),
    };
  }

  /**
   * What the agent has said since `from`, in all its sessions, as much as
   * fits in `budget` bytes of lines and `mostLines` lines; the rest waits
   * for the next read. Null when nothing new has been written. Only whole
   * lines are read from the file still being written, so a message in the
   * middle of being written is left for next time; one an ended session left
   * torn is passed over. The first line is always taken, so one too large
   * for the budget cannot hold up everything after it.
   */
  read(h: Home, from: Cursor, budget: number, mostLines: number): Batch | null {
    const sources = [...new Set(h.sessions().filter((s) => s.stream).map((s) => s.sessionId))];
    let next: Cursor = from;
    const lines: Line[] = [];
    let size = 0;
    let usage: Usage | undefined;
    const tokens: Batch["tokens"] = [];
    const onTo = (c: Cursor): Cursor => ({ ...c, file: c.file + 1, offset: 0, tasks: 0, told: undefined, tokens: claude.NO_TOKENS, byModel: undefined, turns: undefined, activeMs: undefined, pending: {}, limitedUntil: undefined, rejected: undefined, contextTokens: undefined });
    files: while (next.file < sources.length) {
      const sessionId = sources[next.file]!;
      const last = next.file + 1 === sources.length;
      const path = h.events(sessionId);
      if (!existsSync(path)) {
        if (last) break;
        next = onTo(next);
        continue;
      }
      const began = next.offset;
      for (const { bytes, whole, limited } of linesOf(path, next.offset, RAW_LIMIT)) {
        // Being written, or past what one read takes: left for the next read.
        // One longer than a whole read is passed over, or it would hold every read after it.
        if (!whole && limited && next.offset === began) {
          next = { ...next, offset: next.offset + bytes.length };
          break files;
        }
        if (!whole && (last || limited)) break files;
        const r = claude.read(bytes.toString("utf8"));
        const at = isInstant(r.at) ? r.at : now();
        let pending = next.pending;
        if (r.sent) pending = { ...pending, [r.sent.uuid]: r.sent.commandId ?? null };
        const read: Line[] = r.entries.map((e, i) => {
          const line: Line = { seq: next.seq + i, at, sessionId, from: e.from, text: e.text };
          if (e.from === "instruction" && e.uuid) {
            const known = pending[e.uuid];
            // A message the Rust host sent has no record: its uuid is its command's, unless the host made it up.
            const commandId = known !== undefined ? known : claude.hostMade(e.uuid) ? null : e.uuid;
            if (commandId) line.commandId = commandId;
            if (known !== undefined) {
              pending = { ...pending };
              delete pending[e.uuid];
            }
          }
          return line;
        });
        const more = read.reduce((n, l) => n + Buffer.byteLength(JSON.stringify(l)), 0);
        if (lines.length && (size + more > budget || lines.length + read.length > mostLines)) break files;
        size += more;
        next = {
          ...next,
          offset: next.offset + bytes.length,
          seq: next.seq + read.length,
          waiting: r.waiting ?? next.waiting,
          tasks: r.tasks ?? next.tasks,
          told: r.told ?? next.told,
          pending,
          ...limitAfter(next, r),
          ...(r.contextTokens !== undefined ? { contextTokens: r.contextTokens } : {}),
          ...(r.contextUnknown ? { contextTokens: undefined } : {}),
        };
        if (r.durationMs !== undefined) next = { ...next, turns: (next.turns ?? 0) + 1, activeMs: (next.activeMs ?? 0) + r.durationMs };
        if (r.tokens) {
          next = { ...next, tokens: claude.addTokens(next.tokens, r.tokens) };
          if (r.byModel) next = { ...next, byModel: claude.addByModel(next.byModel ?? {}, r.byModel) };
          const held = tokens.at(-1);
          const entry = { sessionId, tokens: next.tokens, ...(next.byModel ? { byModel: next.byModel } : {}) };
          if (held?.sessionId === sessionId) Object.assign(held, entry);
          else tokens.push(entry);
        }
        if (r.usage) usage = { readAt: at, ...r.usage };
        lines.push(...read);
      }
      if (last) break;
      next = onTo(next);
    }
    const moved = next.file !== from.file || next.offset !== from.offset;
    return moved ? { lines, next, ...(usage ? { usage } : {}), tokens } : null;
  }
}

/** Each line of `path` from `offset`, up to `limit` bytes, with whether it ended in a newline. */
function* linesOf(path: string, offset: number, limit: number): Generator<{ bytes: Buffer; whole: boolean; limited: boolean }> {
  const fd = openSync(path, "r");
  try {
    let at = offset;
    let carry = Buffer.alloc(0);
    let read = 0;
    for (;;) {
      const chunk = Buffer.alloc(Math.min(CHUNK, limit - read));
      const n = chunk.length ? readSync(fd, chunk, 0, chunk.length, at) : 0;
      read += n;
      at += n;
      let data = carry.length ? Buffer.concat([carry, chunk.subarray(0, n)]) : chunk.subarray(0, n);
      let start = 0;
      for (let nl = data.indexOf(0x0a, start); nl !== -1; nl = data.indexOf(0x0a, start)) {
        yield { bytes: data.subarray(start, nl + 1), whole: true, limited: false };
        start = nl + 1;
      }
      carry = data.subarray(start);
      if (n === 0 || read >= limit) {
        if (carry.length) yield { bytes: carry, whole: false, limited: read >= limit };
        return;
      }
      data = Buffer.alloc(0);
    }
  } finally {
    closeSync(fd);
  }
}

/**
 * The limit as of `r`: one reached in a turn counts once that turn ends on
 * it, not if the agent works on after it, and a turn begun clears it.
 */
function limitAfter(c: Cursor, r: claude.Read): Pick<Cursor, "limitedUntil" | "rejected"> {
  let { limitedUntil, rejected } = c;
  if (r.turn === "begins") limitedUntil = rejected = undefined;
  if (r.rejected) rejected = r.rejected;
  if (r.turn === "continues") rejected = undefined;
  if (r.turn === "ends") {
    limitedUntil = rejected;
    rejected = undefined;
  }
  return { limitedUntil, rejected };
}

/** Keeps `live`'s turn, tasks and messages not yet taken as of `m`, as a read of its events would. */
function follow(live: Live, m: Message): void {
  const r = claude.read(JSON.stringify(m));
  if (r.waiting !== undefined) live.waiting = r.waiting;
  if (r.tasks !== undefined) live.tasks = r.tasks;
  for (const e of r.entries) {
    if (e.from === "instruction" && e.uuid) live.unechoed = live.unechoed.filter((u) => u.uuid !== e.uuid);
  }
  // A message taken into the turn under way comes back as its words alone, with no uuid; failing them, the oldest is the one taken first.
  const attachment = m.attachment as { type?: unknown; prompt?: unknown } | undefined;
  if (m.type === "attachment" && attachment?.type === "queued_command") {
    const i = live.unechoed.findIndex((u) => u.text === attachment.prompt);
    live.unechoed = live.unechoed.filter((_, j) => j !== (i >= 0 ? i : 0));
  }
}

/** A message as the host keeps it: with the instant it came when it did not say, so a line read again after a restart reads the same. */
export function stamped(m: Message): Message {
  return typeof m.timestamp === "string" ? m : { ...m, timestamp: now() };
}
