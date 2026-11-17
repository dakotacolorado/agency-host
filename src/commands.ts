/**
 * Commands as the host takes them. The service pushes a command when it is
 * written, and again, with every other still pending, each time the socket
 * opens, until a report naming it is in. So the same command arrives more
 * than once and must be acted on once.
 */
import type { Agents, Start } from "./agents.ts";
import type { Context } from "./context.ts";
import type { Effort } from "./sdk.ts";
import { errorText } from "./sys.ts";

export interface CommandReport {
  commandId: string;
  ok: boolean;
  error?: string;
}

/** A command as it comes down, read field by field: unknown fields are ignored, so the service can add one before the host reads it. */
export type Command = { [k: string]: unknown } & { id: string; kind: string };

/** How many settled commands are remembered: far more than are settled between one report and the next. */
const REMEMBERED = 1000;

/** The service takes an error of this many characters at most. */
const LONGEST_ERROR = 4096;

export function report(commandId: string, error?: string): CommandReport {
  return error === undefined ? { commandId, ok: true } : { commandId, ok: false, error: error.slice(0, LONGEST_ERROR) };
}

/**
 * The commands taken, each with its report, so one sent again is answered as
 * before rather than done twice, and one sent again while it is still being
 * done is not started a second time. Held in memory: a host that restarts
 * takes again a command it settled but did not report, which is why each
 * kind is also safe to take again on its own (see `Agents`), but a message,
 * which is said again.
 */
export class Commands {
  private readonly settled = new Map<string, CommandReport>();
  private readonly underway = new Set<string>();
  /** Commands are done one at a time, in the order they came, so a message sent after a start finds its session. */
  private queue: Promise<void> = Promise.resolve();
  private readonly agents: Agents;
  private readonly done: (r: CommandReport, fresh: boolean, kind: string) => void;

  constructor(agents: Agents, done: (r: CommandReport, fresh: boolean, kind: string) => void) {
    this.agents = agents;
    this.done = done;
  }

  /** Takes `command`, unless it was taken already, in which case its report is handed back again; one still underway is passed over. */
  take(command: Command): void {
    const known = this.settled.get(command.id);
    if (known) {
      this.done(known, false, command.kind);
      return;
    }
    if (this.underway.has(command.id)) return;
    this.underway.add(command.id);
    this.queue = this.queue.then(async () => {
      const r = await act(command, this.agents);
      this.underway.delete(command.id);
      this.settled.delete(command.id);
      this.settled.set(command.id, r);
      if (this.settled.size > REMEMBERED) this.settled.delete(this.settled.keys().next().value!);
      this.done(r, true, command.kind);
    });
  }

  /** Resolves once every command taken so far is done. */
  idle(): Promise<void> {
    return this.queue;
  }
}

/** Reads a command off the socket, or says why it is not one. */
export function parse(v: unknown): Command | string {
  if (!v || typeof v !== "object" || Array.isArray(v)) return "a command is an object";
  const c = v as { [k: string]: unknown };
  if (typeof c.id !== "string" || !c.id) return "a command has no id";
  if (typeof c.kind !== "string") return `command ${c.id} has no kind`;
  return c as Command;
}

const str = (v: unknown): string | undefined => (typeof v === "string" ? v : undefined);
const record = (v: unknown): { [k: string]: string } | undefined =>
  v && typeof v === "object" && !Array.isArray(v) ? Object.fromEntries(Object.entries(v).filter(([, x]) => typeof x === "string")) as { [k: string]: string } : undefined;

function contextOf(v: unknown): Context | undefined {
  if (!v || typeof v !== "object") return undefined;
  const c = v as { [k: string]: unknown };
  if (typeof c.key !== "string" || typeof c.version !== "number" || !Array.isArray(c.files)) throw new Error("a context is {key, version, files}");
  const files = c.files.map((f) => {
    const file = (f ?? {}) as { [k: string]: unknown };
    if (typeof file.path !== "string" || typeof file.content !== "string") throw new Error(`${c.key}: a file is {path, content, executable}`);
    return { path: file.path, content: file.content, executable: file.executable === true };
  });
  return { key: c.key, version: c.version, files };
}

const EFFORTS: readonly string[] = ["low", "medium", "high", "xhigh", "max"];
/** An effort Claude Code does not know is refused rather than passed on, where it would fail the session. */
function effortOf(v: unknown): Effort | undefined {
  if (v === undefined || v === null) return undefined;
  if (typeof v !== "string" || !EFFORTS.includes(v)) throw new Error(`${JSON.stringify(v)} is not an effort: ${EFFORTS.join(", ")}`);
  return v as Effort;
}

const KINDS = new Set(["create", "start", "message", "interrupt", "configure", "stop", "delete"]);

/** A kind the host does not handle is reported by name rather than dropped, so the sender learns the command went nowhere. */
export async function act(c: Command, agents: Agents): Promise<CommandReport> {
  const agentId = str(c.agentId);
  const text = str(c.text);
  try {
    if (!KINDS.has(c.kind)) return report(c.id, `this host does not handle ${JSON.stringify(c.kind)} commands`);
    if (!agentId) return report(c.id, `${JSON.stringify(c.kind)} needs an agentId`);
    switch (c.kind) {
      case "create": {
        const name = str(c.name);
        if (!name) return report(c.id, "a create needs a name");
        agents.create(agentId, name);
        break;
      }
      case "start": {
        const sessionId = str(c.sessionId);
        if (!sessionId) return report(c.id, "a start needs a sessionId");
        const documents = Array.isArray(c.documents) ? c.documents.map((d) => contextOf(d)!).filter(Boolean) : undefined;
        const ctx = contextOf(c.context);
        const env = record(c.env);
        const secrets = record(c.secrets);
        const model = str(c.model) || undefined;
        const effort = effortOf(c.effort);
        const mcp = c.mcpServers && typeof c.mcpServers === "object" ? (c.mcpServers as { [k: string]: unknown }) : undefined;
        const s: Start = {
          commandId: c.id,
          agentId,
          sessionId,
          resume: c.resume === true,
          ...(text !== undefined ? { text } : {}),
          ...(ctx ? { context: ctx } : {}),
          ...(documents ? { documents } : {}),
          ...(env ? { env } : {}),
          ...(secrets ? { secrets } : {}),
          ...(mcp ? { mcpServers: mcp } : {}),
          ...(model ? { model } : {}),
          ...(effort ? { effort } : {}),
        };
        agents.start(s);
        break;
      }
      case "message":
        if (text === undefined) return report(c.id, "a message needs text");
        agents.message(agentId, c.id, text);
        break;
      case "interrupt":
        await agents.interrupt(agentId, c.id, text);
        break;
      case "configure": {
        const model = str(c.model);
        if (!model) return report(c.id, "a configure needs a model");
        await agents.configure(agentId, model, effortOf(c.effort) ?? null);
        break;
      }
      case "stop":
        await agents.stop(agentId);
        break;
      case "delete":
        await agents.delete(agentId);
        break;
    }
    return report(c.id);
  } catch (e) {
    return report(c.id, errorText(e));
  }
}
