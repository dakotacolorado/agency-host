/**
 * A deleted agent's records, kept for good in `<archive_dir>/<name>-<id>.tar.zst`
 * on a drive every host mounts, so they outlive the host's own disk.
 */
import { spawn } from "node:child_process";
import { createWriteStream, existsSync } from "node:fs";
import { mkdir, rename, rm, statfs } from "node:fs/promises";
import { dirname, join } from "node:path";
import { pipeline } from "node:stream/promises";
import { createZstdCompress } from "node:zlib";
import type { Record } from "./home.ts";

/** What of an agent's folder is kept: its records, never what a session was given or what it cloned. */
export const KEPT = ["agent.json", "sessions.jsonl", "prompt.md", "logs", "home/.claude"];

/** Left out wherever they are under what is kept: its Claude login, and anything that names itself credentials or MCP servers. */
export const LEFT_OUT = [".credentials*", "credentials.env", "mcp.json", ".mcp.json"];

/** `f_type` of a FUSE filesystem (linux/magic.h). */
const FUSE_SUPER_MAGIC = 0x65735546;

/** Whether `path`, or the nearest folder above it that exists, is on a FUSE mount. */
export async function onFuse(path: string): Promise<boolean> {
  let at = path;
  while (!existsSync(at) && dirname(at) !== at) at = dirname(at);
  return (await statfs(at)).type === FUSE_SUPER_MAGIC;
}

/**
 * Writes the records of the agent whose folder is `folder` (already moved
 * aside, its credentials gone) to `<dir>/<name>-<id>.tar.zst`, through a
 * hidden temporary name so a reader never sees half of one. `dir` must be on
 * the mounted drive: unmounted, it is the root disk, and a copy there is no
 * better kept than the folder itself.
 */
export async function archive(folder: string, record: Record, dir: string, mounted: (path: string) => Promise<boolean> = onFuse): Promise<string> {
  if (!(await mounted(dir))) throw new Error(`${dir} is not on a mounted drive`);
  await mkdir(dir, { recursive: true });
  const name = `${record.name}-${record.id}.tar.zst`;
  const path = join(dir, name);
  const partial = join(dir, `.${name}.partial`);
  const kept = KEPT.filter((k) => existsSync(join(folder, k)));
  const tar = spawn("tar", ["-c", "-C", folder, ...LEFT_OUT.map((p) => `--exclude=${p}`), "--", ...kept], { stdio: ["ignore", "pipe", "pipe"] });
  let stderr = "";
  tar.stderr.on("data", (d: Buffer) => (stderr += d.toString()));
  const exited = new Promise<number | null>((resolve, reject) => {
    tar.on("error", reject);
    tar.on("close", resolve);
  });
  try {
    await pipeline(tar.stdout, createZstdCompress(), createWriteStream(partial, { mode: 0o600 }));
    const code = await exited;
    if (code !== 0) throw new Error(`tar exited ${code}: ${stderr.trim()}`);
    await rename(partial, path);
  } catch (e) {
    tar.kill();
    await rm(partial, { force: true }).catch(() => {});
    throw e;
  }
  return path;
}
