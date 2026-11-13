/**
 * The host: its agents, the commands that come down its socket, and the
 * reports and heartbeats that go up it. Assumes nothing survives but what is
 * in memory, the agents' folders and their sessions: the machine may sleep
 * for weeks between two moments, and what was due is simply late.
 */
import { Agents, START } from "./agents.ts";
import { Commands, parse, type CommandReport } from "./commands.ts";
import type { Config } from "./config.ts";
import { Reporter } from "./report.ts";
import type { StartSession } from "./sdk.ts";
import { Socket, socketUrl } from "./socket.ts";
import { log } from "./sys.ts";

export const HEARTBEAT_MS = 30_000;
/** A change is on the socket within about this long, and reports are no closer together. */
export const BATCH_MS = 250;

export interface HostOptions {
  config: Config;
  version: string;
  startSession: StartSession;
  login: string | null;
  baseEnv: { [k: string]: string };
  heartbeatMs?: number;
  batchMs?: number;
  backoffMs?: [number, number];
}

export class Host {
  readonly agents: Agents;
  readonly commands: Commands;
  private readonly reporter: Reporter;
  private readonly socket: Socket;
  private readonly o: HostOptions;
  private timer: NodeJS.Timeout | null = null;
  private heartbeats: NodeJS.Timeout | null = null;
  private lastFlush = 0;
  private flushing: Promise<void> = Promise.resolve();
  /** The socket the first report has gone up on, so a heartbeat never goes before it. */
  private reportedOn = 0;

  constructor(o: HostOptions) {
    this.o = o;
    this.agents = new Agents({
      dir: o.config.agentsDir,
      startSession: o.startSession,
      permissions: o.config.permissions,
      login: o.login,
      baseEnv: o.baseEnv,
      changed: () => this.soon(),
    });
    this.reporter = new Reporter(this.agents);
    this.commands = new Commands(this.agents, (r, fresh, kind) => this.settled(r, fresh, kind));
    this.socket = new Socket({
      url: socketUrl(o.config.serviceUrl, o.config.hostId),
      token: o.config.token,
      opened: (n) => this.opened(n),
      message: (m) => this.heard(m),
      log,
      ...(o.backoffMs ? { backoffMs: o.backoffMs } : {}),
    });
  }

  start(): void {
    this.agents.reconcile();
    for (const { home, record } of this.agents.all()) {
      const r = this.agents.report(home, record.id, START);
      log(`agent ${record.name}: ${r.state === "working" ? "live" : r.state}`);
    }
    this.socket.start();
    this.heartbeats = setInterval(() => {
      if (this.reportedOn === this.socket.opened) void this.heartbeat();
    }, this.o.heartbeatMs ?? HEARTBEAT_MS);
  }

  async stop(): Promise<void> {
    if (this.heartbeats) clearInterval(this.heartbeats);
    if (this.timer) clearTimeout(this.timer);
    this.socket.close();
    await this.agents.closeAll();
  }

  private settled(r: CommandReport, fresh: boolean, kind: string): void {
    if (fresh) log(`command ${r.commandId} (${kind}): ${r.error ?? "done"}`);
    this.reporter.result(r);
    this.soon();
  }

  private heard(m: unknown): void {
    const msg = (m ?? {}) as { type?: unknown; command?: unknown; error?: unknown };
    if (msg.type === "command") {
      const c = parse(msg.command);
      if (typeof c === "string") log(`socket: ${c}`);
      else this.commands.take(c);
    } else if (msg.type === "error") {
      log(`agency: ${String(msg.error)}`);
    }
    // Anything else is passed over, so the service can add a kind before the host reads it.
  }

  /** On every socket opened: first a full report, then a heartbeat. */
  private async opened(n: number): Promise<void> {
    await this.flush();
    if (this.socket.opened !== n || !this.socket.up) return;
    this.reportedOn = n;
    await this.heartbeat();
  }

  private async heartbeat(): Promise<void> {
    const c = this.o.config;
    await this.socket.send(
      JSON.stringify({
        type: "heartbeat",
        version: this.o.version,
        ...(c.labels ? { labels: c.labels } : {}),
        ...(c.capacity !== undefined ? { capacity: c.capacity } : {}),
      }),
    );
  }

  /** A report within `BATCH_MS`, however many changes come before it. */
  private soon(): void {
    if (this.timer) return;
    const wait = Math.max(0, this.lastFlush + (this.o.batchMs ?? BATCH_MS) - Date.now());
    this.timer = setTimeout(() => {
      this.timer = null;
      void this.flush();
    }, wait);
  }

  /** Sends what the socket has not been told, one report at a time. */
  flush(): Promise<void> {
    this.flushing = this.flushing.then(async () => {
      if (!this.socket.up) return;
      this.lastFlush = Date.now();
      const opened = this.socket.opened;
      const out = this.reporter.next(opened);
      if (!out) return;
      if (await this.socket.send(out.text)) {
        out.sent();
        // A read stops at its budget: there may be more.
        if (out.lines) this.soon();
      }
    }).catch((e: unknown) => log(`report: ${(e as Error).message}`));
    return this.flushing;
  }
}
