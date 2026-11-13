/**
 * The socket: the only way commands come down and reports go up. A socket
 * that is down loses nothing -- the service replays every pending command
 * when one opens, and the host keeps what it has not sent -- it only makes
 * the host slower to hear and be heard until it is back. So this keeps one
 * open whenever it can, hands on every message it reads, and writes what it
 * is given, saying whether it went.
 */
import WebSocket from "ws";

/** Between one failed connection and the next try, doubling from the first to the last; reset once a socket has stayed open for `STEADY_MS`. */
export const BACKOFF_MS: [number, number] = [1000, 60_000];
export const STEADY_MS = 60_000;

/** The service pings every 30 seconds; a socket that hears nothing for this long is taken for dead, as one a machine's sleep cut is. */
const SILENCE_MS = 90_000;

/** How long connecting may take before it is given up on. */
const PATIENCE_MS = 15_000;

/** A socket message may come in many frames, each 32 KB at most: a message is written in frames of this many bytes, leaving room for each frame's header. */
export const FRAME = 31 << 10;

/** The socket's url with this host named: `<service_url with http→ws, https→wss>/socket?hostId=<id>`. */
export function socketUrl(serviceUrl: string, hostId: string): string {
  const url = new URL(`${serviceUrl.replace(/\/+$/, "")}/socket`);
  if (url.protocol === "https:") url.protocol = "wss:";
  else if (url.protocol === "http:") url.protocol = "ws:";
  url.searchParams.set("hostId", hostId);
  return url.toString();
}

export interface SocketOptions {
  url: string;
  token: string;
  /** Called on each socket opened, with its number, before any message read on it is handed on. */
  opened: (n: number) => void | Promise<void>;
  message: (data: unknown) => void;
  log: (m: string) => void;
  backoffMs?: [number, number];
  steadyMs?: number;
}

export class Socket {
  private readonly o: SocketOptions;
  private ws: WebSocket | null = null;
  private count = 0;
  private live = false;
  private stopped = false;
  private delay: number;
  private retry: NodeJS.Timeout | null = null;
  private silence: NodeJS.Timeout | null = null;

  constructor(options: SocketOptions) {
    this.o = options;
    this.delay = this.backoff[0];
  }

  private get backoff(): [number, number] {
    return this.o.backoffMs ?? BACKOFF_MS;
  }

  /** How many sockets have opened, so a reopened one can be told everything. */
  get opened(): number {
    return this.count;
  }

  get up(): boolean {
    return this.live && this.ws?.readyState === WebSocket.OPEN;
  }

  start(): void {
    this.stopped = false;
    this.connect();
  }

  close(): void {
    this.stopped = true;
    if (this.retry) clearTimeout(this.retry);
    if (this.silence) clearTimeout(this.silence);
    this.ws?.terminate();
  }

  private connect(): void {
    this.retry = null;
    // The bearer goes in a header, never in the url, which ends up in logs.
    const ws = new WebSocket(this.o.url, {
      headers: { authorization: `Bearer ${this.o.token}` },
      handshakeTimeout: PATIENCE_MS,
      perMessageDeflate: false,
    });
    this.ws = ws;
    let openedAt = 0;
    // Messages read before `opened` has finished wait for it, so a replayed command is taken after the report on connecting.
    let ready: Promise<void> = Promise.resolve();
    ws.on("open", () => {
      openedAt = Date.now();
      this.live = true;
      this.count++;
      this.hear(ws);
      this.o.log(`socket open: ${this.o.url.replace(/\?.*$/, "")}`);
      ready = Promise.resolve(this.o.opened(this.count)).catch((e) => this.o.log(`socket: ${(e as Error).message}`));
    });
    ws.on("ping", () => this.hear(ws));
    ws.on("message", (data, binary) => {
      this.hear(ws);
      let parsed: unknown;
      try {
        parsed = JSON.parse(binary ? Buffer.from(data as Buffer).toString("utf8") : data.toString());
      } catch {
        this.o.log("socket: a message that is not JSON, passed over");
        return;
      }
      void ready.then(() => this.o.message(parsed));
    });
    ws.on("unexpected-response", (_req, res) => {
      this.o.log(`socket refused: ${res.statusCode}`);
      ws.terminate();
    });
    ws.on("error", (e) => this.o.log(`socket: ${e.message}`));
    ws.on("close", (code) => {
      if (this.ws !== ws) return;
      this.live = false;
      if (this.silence) clearTimeout(this.silence);
      if (this.stopped) return;
      if (openedAt && Date.now() - openedAt >= (this.o.steadyMs ?? STEADY_MS)) this.delay = this.backoff[0];
      if (openedAt) this.o.log(`socket closed (${code}); again in ${this.delay / 1000}s`);
      this.retry = setTimeout(() => this.connect(), this.delay);
      this.delay = Math.min(this.delay * 2, this.backoff[1]);
    });
  }

  private hear(ws: WebSocket): void {
    if (this.silence) clearTimeout(this.silence);
    this.silence = setTimeout(() => {
      this.o.log("socket silent too long: opening another");
      ws.terminate();
    }, SILENCE_MS);
    this.silence.unref();
  }

  /** Writes `text` and says whether it was written: false at once while there is no socket, and false for a write that failed. */
  send(text: string): Promise<boolean> {
    const ws = this.ws;
    if (!ws || !this.up) return Promise.resolve(false);
    const bytes = Buffer.from(text, "utf8");
    return new Promise((resolve) => {
      try {
        for (let at = 0; at < bytes.length || at === 0; at += FRAME) {
          const fin = at + FRAME >= bytes.length;
          const frame = bytes.subarray(at, at + FRAME);
          if (fin) ws.send(frame, { fin: true, binary: false }, (e) => resolve(!e));
          else ws.send(frame, { fin: false, binary: false });
          if (fin) break;
        }
      } catch {
        resolve(false);
      }
    });
  }
}
