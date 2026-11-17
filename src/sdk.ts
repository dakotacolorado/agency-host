/**
 * The one place the host calls the Claude Agent SDK. A session is a
 * `query()` over streaming input: what it is told is pushed into its input,
 * and what it says comes out as SDK messages. The rest of the host sees only
 * `SessionHandle`, so tests drive a stand-in that emits scripted messages.
 */
import { query, type CanUseTool, type EffortLevel, type McpServerConfig, type Query, type SDKUserMessage, type SpawnedProcess } from "@anthropic-ai/claude-agent-sdk";
import { run as runInContainer, type ContainerRun } from "./container.ts";

export type Effort = EffortLevel;

/** An SDK message as the host keeps it: plain JSON, read by `claude.read`. */
export type Message = { [k: string]: unknown };

export interface SessionOptions {
  /** On the host, or with `container`, inside it. */
  cwd: string;
  /** The whole environment of the session's Claude Code: the SDK replaces the subprocess's with it. */
  env: Record<string, string>;
  /** A new conversation under this Claude Code session id... */
  sessionId?: string;
  /** ...or this one carried on. */
  resume?: string;
  mcpServers?: Record<string, unknown>;
  /** The model it runs on, and its effort; absent, Claude Code's defaults. */
  model?: string;
  effort?: Effort;
  /** Answers each tool's permission request: true to allow it, or why not. */
  permit: (tool: string) => true | string;
  /** Whatever the session writes on its stderr. */
  stderr: (text: string) => void;
  /** Runs its Claude Code in this container rather than on the host. */
  container?: ContainerRun;
}

export interface SessionHandle {
  /** Pushes a user message into the session's input; `uuid` comes back on its replay. */
  send(text: string, uuid: string): void;
  /** Stops the turn under way, as Escape does: the session goes on. */
  interrupt(): Promise<void>;
  /** Runs it on `model` and `effort` (null: the model's default) from its next request to the model; a subagent already running keeps its own. */
  configure(model: string, effort: Effort | null): Promise<void>;
  /** Ends the session and its Claude Code. */
  close(): void;
  /** What the session says, until it ends; throws if it ends by failing. */
  events: AsyncIterable<Message>;
}

export type StartSession = (options: SessionOptions) => SessionHandle;

/** An input that waits for the next message until it is closed. */
export class Inbox<T> implements AsyncIterable<T> {
  private queue: T[] = [];
  private wake: (() => void) | null = null;
  private closed = false;

  push(item: T): void {
    if (this.closed) throw new Error("the session's input is closed");
    this.queue.push(item);
    this.wake?.();
  }

  close(): void {
    this.closed = true;
    this.wake?.();
  }

  async *[Symbol.asyncIterator](): AsyncIterator<T> {
    for (;;) {
      const next = this.queue.shift();
      if (next !== undefined) {
        yield next;
        continue;
      }
      if (this.closed) return;
      await new Promise<void>((r) => (this.wake = r));
      this.wake = null;
    }
  }
}

export const startSdkSession: StartSession = (o) => {
  const inbox = new Inbox<SDKUserMessage>();
  const canUseTool: CanUseTool = async (tool, input) => {
    const answer = o.permit(tool);
    return answer === true ? { behavior: "allow", updatedInput: input } : { behavior: "deny", message: answer };
  };
  const q: Query = query({
    prompt: inbox,
    options: {
      cwd: o.cwd,
      env: o.env,
      ...(o.resume ? { resume: o.resume } : { sessionId: o.sessionId }),
      ...(o.model ? { model: o.model } : {}),
      ...(o.effort ? { effort: o.effort } : {}),
      ...(o.mcpServers ? { mcpServers: o.mcpServers as Record<string, McpServerConfig> } : {}),
      // As `claude` itself loads them: the agent's own settings, rules and
      // skills from its CLAUDE_CONFIG_DIR, and those of whatever it works in.
      settingSources: ["user", "project", "local"],
      // Without it the SDK sends an empty system prompt, not Claude Code's.
      systemPrompt: { type: "preset", preset: "claude_code" },
      canUseTool,
      permissionMode: "default",
      // Each message the host sends comes back once Claude Code takes it,
      // which is how a line names the command it carried.
      extraArgs: { "replay-user-messages": null },
      stderr: o.stderr,
      // The image's own `claude`, which is the binary this SDK bundles (src/image.spec.ts).
      ...(o.container ? { spawnClaudeCodeProcess: (s) => runInContainer(o.container!, ["claude", ...s.args], s.env, s.signal, o.stderr) as SpawnedProcess } : {}),
    },
  });
  return {
    send(text, uuid) {
      inbox.push({ type: "user", uuid, session_id: "", parent_tool_use_id: null, message: { role: "user", content: text } } as SDKUserMessage);
    },
    async interrupt() {
      await q.interrupt();
    },
    async configure(model, effort) {
      await q.applyFlagSettings({ model, effortLevel: effort });
    },
    close() {
      inbox.close();
      q.close();
    },
    events: q as AsyncIterable<Message>,
  };
};
