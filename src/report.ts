/**
 * What goes up the socket in a report: every agent's state, the lines not
 * yet written on a socket, command results not yet written, the newest
 * reading of the account's usage, and each session's token totals the
 * socket was not told. Nothing is counted as sent until the write is.
 */
import type { AgentReport, Agents, Cursor, Line, Usage } from "./agents.ts";
import { START } from "./agents.ts";
import type { ByModel, Tokens } from "./claude.ts";
import type { CommandReport } from "./commands.ts";
import { log } from "./sys.ts";

/**
 * Bytes of transcript lines in one report, for every agent together; what
 * does not fit goes in the next. One line may go past it, up to about
 * `claude.LONGEST_TEXT`, so a line longer than the budget is not held up for
 * ever; `REPORT_LIMIT` is what holds the whole report.
 */
export const TRANSCRIPT_PER_REPORT = 32 << 10;

/** The longest report written: a socket message of 128 KB, with room to spare. */
export const REPORT_LIMIT = 120 << 10;

/** The most the service takes of each in one report. */
export const MOST_LINES = 1000;
/** Bytes of command results in one report: an error may be 4 KB, and a thousand of them would not fit a socket message. */
export const RESULTS_PER_REPORT = 32 << 10;
export const MOST_RESULTS = 1000;
export const MOST_TOKENS = 256;

export interface TokenReport extends Tokens {
  agentId: string;
  sessionId: string;
  /** The same counts by model id, subagents included; their sum is the four counts. */
  byModel?: ByModel;
}

/** Each agent's id, where its read began and ended, and what it read. */
interface Read {
  id: string;
  from: Cursor;
  next: Cursor;
  lines: Line[];
}

export interface Outgoing {
  text: string;
  /** Counts what it carried as sent: called once the write is. */
  sent: () => void;
  /** Whether it carried lines, so there may be more to read. */
  lines: boolean;
}

/** Each session's tokens as its lines were read, and what the socket was last told of them, and on which socket. */
export class Totals {
  private readonly read = new Map<string, TokenReport>();
  private told: { on: number; was: Map<string, string> } | null = null;

  add(agentId: string, totals: { sessionId: string; tokens: Tokens; byModel?: ByModel }[]): void {
    for (const t of totals) this.read.set(`${agentId}\n${t.sessionId}`, { agentId, sessionId: t.sessionId, ...t.tokens, ...(t.byModel ? { byModel: t.byModel } : {}) });
  }

  /** Only the agents still here are kept, so a deleted agent's sessions are not told again on every socket. */
  keep(here: (agentId: string) => boolean): void {
    for (const [k, t] of this.read) if (!here(t.agentId)) this.read.delete(k);
  }

  /** The totals socket `opened` has not been told, every one on a socket not told before, as many as one report takes. */
  untold(opened: number): TokenReport[] {
    const told = this.told?.on === opened ? this.told.was : null;
    return [...this.read]
      .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
      .filter(([k, t]) => !told || told.get(k) !== JSON.stringify(t))
      .slice(0, MOST_TOKENS)
      .map(([, t]) => t);
  }

  sent(opened: number, sent: TokenReport[]): void {
    if (this.told?.on !== opened) this.told = { on: opened, was: new Map() };
    for (const t of sent) this.told.was.set(`${t.agentId}\n${t.sessionId}`, JSON.stringify(t));
  }
}

export class Reporter {
  private readonly agents: Agents;
  private unsent: CommandReport[] = [];
  private readonly sentAt = new Map<string, Cursor>();
  private turn = 0;
  /** What the socket was last told of the agents, and on which socket. */
  private told: { on: number; states: string } | null = null;
  /** The newest reading of the account's usage not yet written on a socket. */
  private usage: Usage | null = null;
  private readonly totals = new Totals();
  private readonly limit: number;

  constructor(agents: Agents, limit = REPORT_LIMIT) {
    this.agents = agents;
    this.limit = limit;
  }

  /** A result to report, in place of any not yet sent for the same command. */
  result(r: CommandReport): void {
    const i = this.unsent.findIndex((u) => u.commandId === r.commandId);
    if (i >= 0) this.unsent[i] = r;
    else this.unsent.push(r);
  }

  /**
   * The report for socket `opened`, or null when there is nothing to tell
   * it. A socket not told before is always sent one, which lists every
   * agent. The agents take turns at being read first, so one with much to
   * say holds none of the others up for long.
   */
  next(opened: number): Outgoing | null {
    const all = this.agents.all();
    const first = all.length ? this.turn % all.length : 0;
    this.turn++;
    const order = [...all.slice(first), ...all.slice(0, first)];
    const reads = new Map<string, Read>();
    let room = TRANSCRIPT_PER_REPORT;
    let lineRoom = MOST_LINES;
    const readOrder: string[] = [];
    for (const { home, record } of order) {
      const from = this.sentAt.get(record.id) ?? START;
      const batch = room > 0 && lineRoom > 0 ? this.agents.read(home, from, room, lineRoom) : null;
      if (batch) {
        // Every agent shares the account, so only the newest reading matters.
        if (batch.usage && (!this.usage || this.usage.readAt <= batch.usage.readAt)) this.usage = batch.usage;
        // A total put back by `fit` is the same when read again.
        this.totals.add(record.id, batch.tokens);
      }
      const lines = batch?.lines ?? [];
      room -= lines.reduce((n, l) => n + lineSize(record.id, l), 0);
      lineRoom -= lines.length;
      reads.set(record.id, { id: record.id, from, next: batch?.next ?? from, lines });
      readOrder.push(record.id);
    }
    this.totals.keep((id) => all.some((a) => a.record.id === id));
    const tokens = this.totals.untold(opened);
    const results: CommandReport[] = [];
    let resultBytes = 0;
    for (const r of this.unsent.slice(0, MOST_RESULTS)) {
      resultBytes += Buffer.byteLength(JSON.stringify(r));
      if (results.length && resultBytes > RESULTS_PER_REPORT) break;
      results.push(r);
    }
    const usage = this.usage;
    const statesOf = () => all.map(({ home, record }) => this.agents.report(home, record.id, reads.get(record.id)!.next));
    const build = (states: AgentReport[]) => {
      const lines = all.flatMap(({ record }) => reads.get(record.id)!.lines.map((l) => ({ agentId: record.id, ...l })));
      return JSON.stringify({
        type: "report",
        agents: states,
        lines,
        commandResults: results,
        ...(tokens.length ? { tokens } : {}),
        ...(usage ? { usage } : {}),
      });
    };
    // A report too long for the socket is closed on once written, so it would
    // be counted sent and lost: the lines read last are put back, their
    // agents' reads undone to go in a later report, until it fits. The states
    // are worked out again each time, since they follow what was read.
    let states = statesOf();
    let text = build(states);
    for (const id of [...readOrder].reverse()) {
      if (Buffer.byteLength(text) <= this.limit) break;
      const r = reads.get(id)!;
      if (!r.lines.length) continue;
      log(`report too long for the socket: ${id}'s ${r.lines.length} lines wait`);
      reads.set(id, { ...r, next: r.from, lines: [] });
      states = statesOf();
      text = build(states);
    }
    const statesText = JSON.stringify(states);
    const changed = !this.told || this.told.on !== opened || this.told.states !== statesText;
    const said = [...reads.values()].some((r) => r.lines.length);
    if (!(changed || said || results.length || usage || tokens.length)) {
      // Bytes read that said nothing need no report to be passed.
      for (const r of reads.values()) this.sentAt.set(r.id, r.next);
      return null;
    }
    return {
      text,
      lines: said,
      sent: () => {
        this.unsent = this.unsent.filter((u) => !results.includes(u));
        if (this.usage === usage) this.usage = null;
        this.totals.sent(opened, tokens);
        for (const r of reads.values()) this.sentAt.set(r.id, r.next);
        for (const id of this.sentAt.keys()) if (!reads.has(id)) this.sentAt.delete(id);
        this.told = { on: opened, states: statesText };
      },
    };
  }
}

/** A line's bytes in a report, where it goes with its agent's id. */
function lineSize(agentId: string, line: Line): number {
  return Buffer.byteLength(JSON.stringify({ agentId, ...line }));
}
