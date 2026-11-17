/**
 * An agent's folder on this host, `<agents_dir>/<name>/`, and everything the
 * host keeps about the agent there:
 *
 * ```text
 * agent.json        who it is: id and name, as created
 * sessions.jsonl    each session the host started for it, each it stopped, and each a drain closed
 * prompt.md         the opening message of its latest session
 * env.json         its latest start's variables, the addresses of its services: never a token
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
import { appendFileSync, existsSync, mkdirSync, readdirSync, readFileSync, renameSync, rmSync, utimesSync, writeFileSync } from "node:fs";
import { chmod, lstat, readdir, rm, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";

/** Hidden, so it is never taken for an agent named so. */
export const DELETED = ".deleted";

/** Where agents deleted before deletion had its name were set aside: pruned as `.deleted` is. */
const RETIRED = ".retired";

/** The newest this many deleted agents' folders are kept, however old; the rest are removed. */
export const KEPT_MOST = 100;

/** What a deleted agent's folder keeps of what a session was given: nothing. */
const GIVEN = ["credentials.env", "env.json", "mcp.json"];

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
  /** Closed by a drain between its turns, and not resumed since: it lost nothing. */
  drained?: boolean;
  /** The model and effort its start named, or a `configure` since did; absent, Claude Code's defaults. Kept so the session resumes on them. */
  model?: string;
  effort?: string;
  /** Run in a container (`container.ts`) rather than on the host, as its start said; absent, on the host. Kept so the session resumes where it ran. */
  runtime?: "container";
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

  /** The addresses its latest start gave it, kept for a session the host resumes itself. Tokens are never here. */
  env(): string {
    return join(this.dir, "env.json");
  }

  /** What older hosts kept its tokens in: read once and removed (`Agents`). */
  legacyCredentials(): string {
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
          ...(typeof e.model === "string" ? { model: e.model } : {}),
          ...(typeof e.effort === "string" ? { effort: e.effort } : {}),
          ...(e.runtime === "container" ? { runtime: "container" as const } : {}),
          stopped: false,
          ended: false,
        });
      } else if (e.stopped === true) {
        for (const s of sessions) if (s.sessionId === id) s.stopped = true;
      } else if (e.ended === true) {
        for (const s of sessions) if (s.sessionId === id) s.ended = true;
      } else if (typeof e.drained === "boolean") {
        for (const s of sessions) {
          if (s.sessionId !== id) continue;
          if (e.drained) s.drained = true;
          else delete s.drained;
        }
      } else if (e.configured === true && typeof e.model === "string") {
        for (const s of sessions) {
          if (s.sessionId !== id) continue;
          s.model = e.model;
          if (typeof e.effort === "string") s.effort = e.effort;
          else delete s.effort;
        }
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
    const { stopped: _s, ended: _e, drained: _d, ...line } = session;
    this.append(line);
  }

  stopped(sessionId: string): void {
    this.append({ sessionId, stopped: true });
  }

  ended(sessionId: string): void {
    this.append({ sessionId, ended: true });
  }

  /** Records the model and effort (null: the model's default) a `configure` switched the session to. */
  configured(sessionId: string, model: string, effort: string | null): void {
    this.append({ sessionId, configured: true, model, ...(effort ? { effort } : {}) });
  }

  /** Marks a session closed between its turns by a drain, or, with false, resumed since. */
  drained(sessionId: string, drained = true): void {
    this.append({ sessionId, drained });
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

  /**
   * Takes away its credentials and MCP servers, then moves the folder to
   * `<agents_dir>/.deleted/<name>-<id>`, so a new agent can be given its
   * name. It is kept there for a while, until `prune` removes it.
   */
  delete(record: Record): string {
    for (const f of GIVEN) rmSync(join(this.dir, f), { force: true });
    const parent = dirname(this.dir);
    mkdirSync(join(parent, DELETED), { recursive: true });
    const aside = join(parent, DELETED, `${record.name}-${record.id}`);
    renameSync(this.dir, aside);
    // Dated by its deletion, not its last write, so it is kept for its full time.
    const t = new Date();
    utimesSync(aside, t, t);
    return aside;
  }
}

/** Every agent with a folder under `dir`, deleted ones aside, by name. */
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

export function deleted(dir: string, id: string): boolean {
  return [DELETED, RETIRED].some((aside) => {
    try {
      return readdirSync(join(dir, aside)).some((n) => n.endsWith(`-${id}`));
    } catch {
      return false;
    }
  });
}

/**
 * Removes the folders of deleted agents past the newest `keptMost`, from
 * `.deleted` and `.retired`; and takes the
 * credentials and MCP servers from those kept. Each folder removed leaves an
 * empty file by its name, so its agent is still known deleted. A folder that
 * will not go is left, and named in what this returns, to be tried again.
 */
export async function prune(dir: string, keptMost = KEPT_MOST): Promise<{ removed: string[]; failed: string[] }> {
  const folders: { path: string; at: number }[] = [];
  for (const aside of [DELETED, RETIRED]) {
    let names: string[];
    try {
      names = await readdir(join(dir, aside));
    } catch {
      continue;
    }
    for (const n of names) {
      const path = join(dir, aside, n);
      try {
        const s = await lstat(path);
        if (s.isDirectory()) folders.push({ path, at: s.mtimeMs });
      } catch {
        // Gone already.
      }
    }
  }
  folders.sort((a, b) => b.at - a.at);
  const removed: string[] = [];
  const failed: string[] = [];
  for (const [i, f] of folders.entries()) {
    if (i < keptMost) {
      for (const g of GIVEN) await rm(join(f.path, g), { force: true }).catch(() => {});
      continue;
    }
    try {
      await removeAll(f.path);
      await writeFile(f.path, "");
      removed.push(f.path);
    } catch {
      failed.push(f.path);
    }
  }
  return { removed, failed };
}

/** Removes `path` and all it holds, opening up any folder an install left read-only first if it must. */
async function removeAll(path: string): Promise<void> {
  try {
    await rm(path, { recursive: true, force: true });
  } catch {
    await writable(path);
    await rm(path, { recursive: true, force: true });
  }
}

async function writable(path: string): Promise<void> {
  if (!(await lstat(path).catch(() => null))?.isDirectory()) return;
  await chmod(path, 0o700).catch(() => {});
  for (const n of await readdir(path).catch(() => [] as string[])) await writable(join(path, n));
}
