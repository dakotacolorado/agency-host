/** A stand-in for the SDK, for tests: sessions that say what a test scripts. */
import { mkdtempSync, readFileSync, realpathSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Inbox, type Message, type SessionHandle, type SessionOptions, type StartSession } from "./sdk.ts";

export class FakeSession implements SessionHandle {
  readonly options: SessionOptions;
  readonly sent: { text: string; uuid: string }[] = [];
  interrupts = 0;
  closed = false;
  private readonly out = new Inbox<Message | Error>();
  /** What the session does with each message: by default it takes it and answers, as a turn. */
  onSend: (text: string, uuid: string, s: FakeSession) => void = (text, uuid, s) => s.turn(text, uuid, `said: ${text}`);

  constructor(options: SessionOptions) {
    this.options = options;
  }

  send(text: string, uuid: string): void {
    if (this.closed) throw new Error("closed");
    this.sent.push({ text, uuid });
    this.onSend(text, uuid, this);
  }

  async interrupt(): Promise<void> {
    this.interrupts++;
    this.emit({ type: "result", subtype: "error_during_execution", is_error: true });
  }

  close(): void {
    this.closed = true;
    this.out.close();
  }

  emit(m: Message): void {
    this.out.push(m);
  }

  /** The session ends by itself, as a crash does. */
  fail(why = "it crashed"): void {
    this.out.push(new Error(why));
  }

  /** A whole turn: begun, the message taken, an answer, its end. */
  turn(text: string, uuid: string, answer: string, tokens = 1): void {
    this.emit({ type: "system", subtype: "init", session_id: "s" });
    this.emit({ type: "user", message: { role: "user", content: text }, uuid, isReplay: true, parent_tool_use_id: null });
    this.emit({ type: "assistant", message: { content: [{ type: "text", text: answer }], stop_reason: null }, parent_tool_use_id: null });
    this.emit({
      type: "result",
      subtype: "success",
      model_usage_since_last_result: { m: { inputTokens: tokens, outputTokens: 10 * tokens, cacheCreationInputTokens: 0, cacheReadInputTokens: 100 * tokens } },
    });
  }

  get events(): AsyncIterable<Message> {
    const out = this.out;
    return {
      async *[Symbol.asyncIterator]() {
        for await (const m of out) {
          if (m instanceof Error) throw m;
          yield m;
        }
      },
    };
  }
}

export class FakeSdk {
  readonly sessions: FakeSession[] = [];
  /** Set to make the next start throw, as an SDK that cannot run does. */
  refuse: string | null = null;
  /** Applied to each session as it starts. */
  setup: (s: FakeSession) => void = () => {};

  readonly start: StartSession = (options) => {
    if (this.refuse) throw new Error(this.refuse);
    const s = new FakeSession(options);
    this.setup(s);
    this.sessions.push(s);
    return s;
  };

  get last(): FakeSession {
    const s = this.sessions.at(-1);
    if (!s) throw new Error("no session started");
    return s;
  }
}

export function scratch(name: string): string {
  return realpathSync(mkdtempSync(join(tmpdir(), `agency-host-${name}-`)));
}

/** Lets every pending callback and promise run. */
export function settle(ms = 20): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

export async function until(check: () => boolean, ms = 3000): Promise<void> {
  const end = Date.now() + ms;
  while (!check()) {
    if (Date.now() > end) throw new Error("timed out waiting");
    await settle(10);
  }
}

/** A turn a usage limit ended, as a session logged it: the refusing `rate_limit_event`, Claude Code's notice, and the `result`. Its limit resets at 2026-11-15T08:50:00Z. */
export const LIMIT_STOP: Message[] = readFileSync(new URL("./fixtures/limit-stop.jsonl", import.meta.url), "utf8")
  .split("\n")
  .filter(Boolean)
  .map((l) => JSON.parse(l) as Message);
