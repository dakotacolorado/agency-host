/**
 * An agent's folder on this host, `<agents_dir>/<name>/`, and everything the
 * host keeps about the agent there:
 *
 * ```text
 * agent.json        who it is: id and name, as created
 * sessions.jsonl    each session the host started for it, and each it stopped
 * prompt.md         the opening message of its latest session
 * credentials.env   its latest start's variables and tokens, mode 600
 * mcp.json          its latest start's MCP servers, their ${VAR}s unfilled
 * home/.claude/     its own Claude Code: settings, rules, skills, transcripts
 * logs/<id>.jsonl   every message each session's SDK stream gave, as it came
 * logs/<id>.log     everything each session wrote on its stderr
 * ```
 *
 * The host remembers nothing between runs that is not here, so a machine
 * that dies at any point leaves a folder the next run can read. The shapes
 * are the Rust host's, so either can read what the other wrote.
 */
import { appendFileSync, existsSync, mkdirSync, readdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

/** Hidden, so it is never taken for an agent named so. */
export const RETIRED = ".retired";

export interface Record {
  id: string;
  name: string;
}

/** One session the host started for an agent. `claude` is the Claude Code session it carries. */
export interface Session {
  sessionId: string;
  claude: string;
  /** Read by the Rust host, which also carried sessions in Claude Code's own transcript; always 0 and true here. */
  from: number;
  stream: boolean;
  /** The uuid the start's own first message went in with, when it had one. */
  opening?: string;
  /** Ended by a `stop`, rather than a crash or the machine going down. */
  stopped: boolean;
  /** Seen by the host to end without a stop. A session neither stopped nor ended was live when the host went, and is resumed. */
  ended: boolean;
}

/** An agent name as the service allows it, which is also all that keeps a name from reaching outside the agents folder. */
export function validName(name: string): boolean {
  return /^[a-z0-9][a-z0-9-]{0,39}$/.test(name);
}

export class Home {
  readonly dir: string;

  constructor(dir: string) {
    this.dir = dir;
  }

  /** The agent's `CLAUDE_CONFIG_DIR`. */
  claude(): string {
    return join(this.dir, "home", ".claude");
  }

  events(sessionId: string): string {
    return join(this.dir, "logs", `${sessionId}.jsonl`);
  }

  errors(sessionId: string): string {
    return join(this.dir, "logs", `${sessionId}.log`);
  }

  prompt(): string {
    return join(this.dir, "prompt.md");
  }

  credentials(): string {
    return join(this.dir, "credentials.env");
  }

  mcp(): string {
    return join(this.dir, "mcp.json");
  }

  private recordFile(): string {
    return join(this.dir, "agent.json");
  }

  private sessionsFile(): string {
    return join(this.dir, "sessions.jsonl");
  }

  record(): Record | null {
    try {
      const r = JSON.parse(readFileSync(this.recordFile(), "utf8")) as Partial<Record>;
      return typeof r.id === "string" && typeof r.name === "string" ? { id: r.id, name: r.name } : null;
    } catch {
      return null;
    }
  }

  /**
   * Makes the folder for `record`, or takes up the one a create that was cut
   * off, or sent again, left. One that belongs to another agent is refused:
   * its folder is not this host's to take.
   */
  make(record: Record): void {
    const held = this.record();
    if (held && held.id !== record.id) throw new Error(`${this.dir} already holds agent ${held.id} by that name`);
    mkdirSync(join(this.dir, "home"), { recursive: true });
    mkdirSync(join(this.dir, "logs"), { recursive: true });
    writeFileSync(this.recordFile(), `${JSON.stringify(record, null, 2)}\n`);
  }

  /** Every session the host has started for the agent, oldest first. A line torn by the machine going down mid-write is passed over. */
  sessions(): Session[] {
    let text = "";
    try {
      text = readFileSync(this.sessionsFile(), "utf8");
    } catch {
      return [];
    }
    const sessions: Session[] = [];
    for (const line of text.split("\n")) {
      let e: { [k: string]: unknown };
      try {
        e = JSON.parse(line);
      } catch {
        continue;
      }
      if (!e || typeof e !== "object" || typeof e.sessionId !== "string") continue;
      const id = e.sessionId;
      if (typeof e.claude === "string") {
        sessions.push({
          sessionId: id,
          claude: e.claude,
          from: typeof e.from === "number" ? e.from : 0,
          stream: e.stream === true,
          ...(typeof e.opening === "string" ? { opening: e.opening } : {}),
          stopped: false,
          ended: false,
        });
      } else if (e.stopped === true) {
        for (const s of sessions) if (s.sessionId === id) s.stopped = true;
      } else if (e.ended === true) {
        for (const s of sessions) if (s.sessionId === id) s.ended = true;
      } else if (e.withdrawn === true) {
        for (let i = sessions.length - 1; i >= 0; i--) if (sessions[i]!.sessionId === id) sessions.splice(i, 1);
      }
    }
    return sessions;
  }

  latest(): Session | null {
    return this.sessions().at(-1) ?? null;
  }

  begin(session: Session): void {
    const { stopped: _s, ended: _e, ...line } = session;
    this.append(line);
  }

  stopped(sessionId: string): void {
    this.append({ sessionId, stopped: true });
  }

  ended(sessionId: string): void {
    this.append({ sessionId, ended: true });
  }

  /** Takes back a session that would not start, so the start is tried again when it is sent again. */
  withdraw(sessionId: string): void {
    this.append({ sessionId, withdrawn: true });
  }

  private append(event: object): void {
    const file = this.sessionsFile();
    let line = `${JSON.stringify(event)}\n`;
    // After a line torn by the machine going down, this one starts afresh.
    if (existsSync(file)) {
      const text = readFileSync(file);
      if (text.length && text[text.length - 1] !== 0x0a) line = `\n${line}`;
    }
    appendFileSync(file, line);
  }

  /** Moves the folder to `<agents_dir>/.retired/<name>-<id>`, where it is kept, so a new agent can be given its name. */
  retire(record: Record): void {
    const parent = dirname(this.dir);
    mkdirSync(join(parent, RETIRED), { recursive: true });
    renameSync(this.dir, join(parent, RETIRED, `${record.name}-${record.id}`));
  }
}

/** Every agent with a folder under `dir`, retired ones aside, by name. */
export function all(dir: string): { home: Home; record: Record }[] {
  let names: string[];
  try {
    names = readdirSync(dir);
  } catch {
    return [];
  }
  return names
    .filter((n) => !n.startsWith("."))
    .map((n) => new Home(join(dir, n)))
    .flatMap((home) => {
      const record = home.record();
      return record ? [{ home, record }] : [];
    })
    .sort((a, b) => (a.record.name < b.record.name ? -1 : a.record.name > b.record.name ? 1 : 0));
}

export function retired(dir: string, id: string): boolean {
  try {
    return readdirSync(join(dir, RETIRED)).some((n) => n.endsWith(`-${id}`));
  } catch {
    return false;
  }
}
