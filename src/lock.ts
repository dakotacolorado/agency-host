/**
 * One host per agents folder: two would each take every command and run it
 * twice. Node has no flock, and a pid file outlives a machine unplugged,
 * whose pid the next boot may give another process; so the lock is a unix
 * socket the host listens on, which the system lets go when the process
 * ends, however it ends. A socket file left behind answers no one, and is
 * taken over.
 */
import { createHash } from "node:crypto";
import { mkdirSync, unlinkSync } from "node:fs";
import { createConnection, createServer, type Server } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

/** A unix socket's path is short on macOS (104 bytes); one too long for it goes in the temp folder, named by the folder it locks. */
const LONGEST_PATH = 100;

export function lockPath(agentsDir: string): string {
  const beside = join(agentsDir, ".agent-host.sock");
  if (Buffer.byteLength(beside) <= LONGEST_PATH) return beside;
  return join(tmpdir(), `agent-host-${createHash("sha256").update(agentsDir).digest("hex").slice(0, 16)}.sock`);
}

export async function hold(agentsDir: string): Promise<Server> {
  mkdirSync(agentsDir, { recursive: true });
  const path = lockPath(agentsDir);
  const listen = () =>
    new Promise<Server>((resolve, reject) => {
      const server = createServer((c) => c.end());
      server.once("error", reject);
      server.listen(path, () => {
        server.off("error", reject);
        server.unref();
        resolve(server);
      });
    });
  try {
    return await listen();
  } catch (e) {
    if ((e as NodeJS.ErrnoException).code !== "EADDRINUSE") throw e;
  }
  const answered = await new Promise<boolean>((resolve) => {
    const c = createConnection(path);
    c.once("connect", () => {
      c.destroy();
      resolve(true);
    });
    c.once("error", () => resolve(false));
  });
  if (answered) throw new Error(`another agent-host is already running on ${agentsDir}: stop it first`);
  unlinkSync(path);
  return listen();
}
