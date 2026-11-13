import type { IncomingMessage } from "node:http";
import type { AddressInfo } from "node:net";
import { afterEach, describe, expect, it } from "vitest";
import { WebSocketServer, type WebSocket } from "ws";
import { Host } from "./host.ts";
import { Socket, socketUrl } from "./socket.ts";
import { FakeSdk, scratch, until } from "./testing.ts";

type Up = { type: string; [k: string]: unknown };

/** agent-service's side, as far as a host sees it. */
class Service {
  readonly wss: WebSocketServer;
  readonly sockets: { ws: WebSocket; req: IncomingMessage; got: Up[] }[] = [];

  constructor() {
    this.wss = new WebSocketServer({ port: 0, host: "127.0.0.1" });
    this.wss.on("connection", (ws, req) => {
      const s = { ws, req, got: [] as Up[] };
      this.sockets.push(s);
      ws.on("message", (data) => s.got.push(JSON.parse(data.toString()) as Up));
    });
  }

  static async open(): Promise<Service> {
    const s = new Service();
    await new Promise<void>((r) => s.wss.once("listening", () => r()));
    cleanup.push(() => s.close());
    return s;
  }

  get url(): string {
    return `http://127.0.0.1:${(this.wss.address() as AddressInfo).port}`;
  }

  get last() {
    return this.sockets.at(-1)!;
  }

  command(command: object, s = this.last): void {
    s.ws.send(JSON.stringify({ type: "command", command }));
  }

  results(s = this.last): unknown[] {
    return s.got.filter((m) => m.type === "report").flatMap((m) => m.commandResults as unknown[]);
  }

  close(): Promise<void> {
    for (const s of this.sockets) s.ws.terminate();
    return new Promise((r) => this.wss.close(() => r()));
  }
}

let cleanup: (() => Promise<void>)[] = [];
afterEach(async () => {
  for (const c of cleanup.reverse()) await c();
  cleanup = [];
});

function start(service: Service, sdk = new FakeSdk()) {
  const host = new Host({
    config: { serviceUrl: service.url, token: "t0k3n", hostId: "studio", capacity: 2, labels: ["linux"], agentsDir: scratch("host"), permissions: { allow: ["*"], deny: [] } },
    version: "9.9.9",
    startSession: sdk.start,
    login: null,
    baseEnv: {},
    batchMs: 10,
    backoffMs: [20, 100],
  });
  host.start();
  cleanup.push(() => host.stop());
  return { host, sdk };
}

describe("the socket's address", () => {
  it("is the service's, http to ws and https to wss, naming the host", () => {
    expect(socketUrl("https://agents.example/", "studio")).toBe("wss://agents.example/socket?hostId=studio");
    expect(socketUrl("http://127.0.0.1:8080", "a b")).toBe("ws://127.0.0.1:8080/socket?hostId=a+b");
  });
});

describe("a host on its socket", () => {
  it("connects with its bearer and sends a full report, then a heartbeat", async () => {
    const service = await Service.open();
    start(service);
    await until(() => service.sockets.length === 1 && service.last.got.length >= 2);
    const { req, got } = service.last;
    expect(req.url).toBe("/socket?hostId=studio");
    expect(req.headers.authorization).toBe("Bearer t0k3n");
    expect(got[0]).toEqual({ type: "report", agents: [], lines: [], commandResults: [] });
    expect(got[1]).toEqual({ type: "heartbeat", version: "9.9.9", labels: ["linux"], capacity: 2 });
  });

  it("takes a command replayed once, reports it, and reports again what it changed", async () => {
    const service = await Service.open();
    const { sdk } = start(service);
    await until(() => service.last?.got.length >= 2);
    const create = { id: "c1", kind: "create", agentId: "w1", name: "ana" };
    service.command(create);
    service.command(create);
    service.command({ id: "c2", kind: "start", agentId: "w1", sessionId: "s1", resume: false, text: "hello" });
    await until(() => service.results().length >= 2 && service.last.got.some((m) => m.type === "report" && (m.lines as unknown[]).length >= 2));
    expect(sdk.sessions).toHaveLength(1);
    const reports = service.last.got.filter((m) => m.type === "report");
    const lines = reports.flatMap((m) => m.lines as { agentId: string; seq: number; from: string; text: string; commandId?: string }[]);
    expect(lines.map((l) => [l.agentId, l.seq, l.from, l.text, l.commandId])).toEqual([
      ["w1", 0, "instruction", "hello", "c2"],
      ["w1", 1, "agent", "said: hello", undefined],
    ]);
    expect((reports.at(-1)!.agents as { state: string }[])[0]!.state).toBe("waiting");
    // The duplicate is answered again, never done twice.
    expect(new Set(service.results().map((r) => JSON.stringify(r)))).toEqual(new Set([JSON.stringify({ commandId: "c1", ok: true }), JSON.stringify({ commandId: "c2", ok: true })]));
  });

  it("opens another socket when one closes, sending its report then heartbeat again, and takes what is replayed there once", async () => {
    const service = await Service.open();
    const { sdk } = start(service);
    await until(() => service.last?.got.length >= 2);
    service.command({ id: "c1", kind: "create", agentId: "w1", name: "ana" });
    service.command({ id: "c2", kind: "start", agentId: "w1", sessionId: "s1", resume: false, text: "hi" });
    await until(() => service.results().length >= 2);
    service.last.ws.terminate();
    await until(() => service.sockets.length === 2 && service.last.got.length >= 2);
    const [report, heartbeat] = service.last.got;
    expect(report!.type).toBe("report");
    expect(report!.agents).toEqual([{ id: "w1", state: "waiting", sessionId: "s1", tasks: 0, told: true }]);
    // Lines that went up on the last socket are not sent again.
    expect(report!.lines).toEqual([]);
    expect(heartbeat!.type).toBe("heartbeat");
    // Replayed, as the service does after the host's first message.
    service.command({ id: "c2", kind: "start", agentId: "w1", sessionId: "s1", resume: false, text: "hi" });
    service.command({ id: "c3", kind: "message", agentId: "w1", text: "again" });
    await until(() => service.results().length >= 2);
    expect(sdk.sessions).toHaveLength(1);
    expect(sdk.last.sent.map((s) => s.text)).toEqual(["hi", "again"]);
  });

  it("reports a command it cannot do, and passes over a message it does not know", async () => {
    const service = await Service.open();
    start(service);
    await until(() => service.last?.got.length >= 2);
    service.last.ws.send(JSON.stringify({ type: "agent", agent: {} }));
    service.last.ws.send("not json");
    service.command({ id: "c9", kind: "message", agentId: "w9", text: "hi" });
    await until(() => service.results().length >= 1);
    expect(service.results()).toEqual([{ commandId: "c9", ok: false, error: "agent w9 has no live session on this host" }]);
  });
});

/** The payload lengths of the WebSocket frames in `wire`, a client's (masked) bytes. */
function frameSizes(wire: Buffer): { fin: boolean; opcode: number; size: number }[] {
  const out = [];
  for (let at = 0; at + 2 <= wire.length; ) {
    const fin = (wire[at]! & 0x80) !== 0;
    const opcode = wire[at]! & 0x0f;
    let size = wire[at + 1]! & 0x7f;
    let head = 2;
    if (size === 126) {
      size = wire.readUInt16BE(at + 2);
      head = 4;
    } else if (size === 127) {
      size = Number(wire.readBigUInt64BE(at + 2));
      head = 10;
    }
    if (wire[at + 1]! & 0x80) head += 4;
    out.push({ fin, opcode, size });
    at += head + size;
  }
  return out;
}

describe("the socket", () => {
  it("writes a long message whole, in frames of 32 KB at most, and says it went; false while it is down", async () => {
    const service = await Service.open();
    const socket = new Socket({ url: socketUrl(service.url, "h"), token: "t", opened: () => {}, message: () => {}, log: () => {} });
    expect(await socket.send("{}")).toBe(false);
    socket.start();
    cleanup.push(async () => socket.close());
    await until(() => socket.up && service.sockets.length === 1);
    const chunks: Buffer[] = [];
    const raw = (service.last.ws as unknown as { _socket: NodeJS.EventEmitter })._socket;
    raw.prependListener("data", (b: Buffer) => chunks.push(Buffer.from(b)));
    const text = JSON.stringify({ type: "report", lines: ["z".repeat(100 << 10)] });
    expect(await socket.send(text)).toBe(true);
    await until(() => service.last.got.length === 1);
    expect(service.last.got[0]).toEqual(JSON.parse(text));
    const frames = frameSizes(Buffer.concat(chunks));
    expect(frames.length).toBe(4);
    expect(frames.every((f) => f.size <= 32 << 10)).toBe(true);
    expect(frames.map((f) => [f.fin, f.opcode])).toEqual([[false, 1], [false, 0], [false, 0], [true, 0]]);
  });
});
