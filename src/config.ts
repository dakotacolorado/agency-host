import { existsSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { isAbsolute, join } from "node:path";
import { parse } from "smol-toml";
import { hostname } from "./sys.ts";

export interface Config {
  serviceUrl: string;
  token: string;
  hostId: string;
  /** Unset, the heartbeat leaves it out and the service places nothing here: room is the owner's to state, not the host's to deduce from the machine. */
  capacity?: number;
  labels?: string[];
  permissions: Permissions;
  /** Two hosts on one machine need two of these, or each reports the other's agents. */
  agentsDir: string;
  /** How long a drain waits for sessions to finish their turns before it closes the rest as a stop of the host does. */
  drainSeconds: number;
  /** Where a deleted agent's records are kept for good, on a drive every host mounts. Unset, they are kept only in `.deleted`. */
  archiveDir?: string;
}

/**
 * The rule a permission request is answered by: a tool named in `deny` is
 * refused, then one named in `allow` is given, and any other refused. A name
 * ending in `*` is every tool it begins. Unset, every tool is allowed.
 */
export interface Permissions {
  allow: string[];
  deny: string[];
}

export const DRAIN_SECONDS = 1800;

export const EVERY_TOOL: Permissions = { allow: ["*"], deny: [] };

export function allows(permissions: Permissions, tool: string): boolean {
  const names = (patterns: string[]) =>
    patterns.some((p) => (p.endsWith("*") ? tool.startsWith(p.slice(0, -1)) : p === tool));
  return !names(permissions.deny) && names(permissions.allow);
}

/** `tmux_socket` was read by an older release, and is accepted so a config that still names it is not refused. */
const KEYS = new Set(["service_url", "token", "host_id", "capacity", "labels", "agents_dir", "drain_seconds", "archive_dir", "permissions", "tmux_socket"]);

/**
 * `agency-host/config.toml` in the config folder, or, when only that exists,
 * `agent-host/config.toml`, where agency-host's releases as agent-host kept it.
 */
export function defaultPath(env: NodeJS.ProcessEnv = process.env, exists: (path: string) => boolean = existsSync): string | null {
  const base = env.XDG_CONFIG_HOME || (env.HOME ? join(env.HOME, ".config") : null);
  if (!base) return null;
  const path = join(base, "agency-host", "config.toml");
  const old = join(base, "agent-host", "config.toml");
  return !exists(path) && exists(old) ? old : path;
}

/** Reads the file, which is the only place settings come from. */
export function load(path: string | null): Config {
  const file = path ?? defaultPath();
  if (!file) throw new Error("no config file: pass --config PATH");
  let text: string;
  try {
    text = readFileSync(file, "utf8");
  } catch (e) {
    throw new Error(`${file}: ${(e as Error).message}`);
  }
  try {
    return fromText(text, process.env.HOME ?? homedir());
  } catch (e) {
    throw new Error(`${file}: ${(e as Error).message}`);
  }
}

export function fromText(text: string, home: string | null): Config {
  const file = parse(text) as Record<string, unknown>;
  const unknown = Object.keys(file).find((k) => !KEYS.has(k));
  if (unknown) throw new Error(`unknown setting ${unknown}`);
  const string = (key: string): string | undefined => {
    const v = file[key];
    if (v === undefined) return undefined;
    if (typeof v !== "string") throw new Error(`${key} is not a string`);
    return v;
  };
  const strings = (v: unknown, key: string): string[] | undefined => {
    if (v === undefined) return undefined;
    if (!Array.isArray(v) || v.some((s) => typeof s !== "string")) throw new Error(`${key} is not a list of strings`);
    return v as string[];
  };
  const serviceUrl = string("service_url");
  if (!serviceUrl) throw new Error("no service_url");
  const token = string("token");
  if (!token) throw new Error("no token");
  const hostId = string("host_id") ?? hostname();
  if (!hostId) throw new Error("no host_id, and no hostname to use instead");
  const capacity = file.capacity;
  if (capacity !== undefined && !(Number.isInteger(Number(capacity)) && Number(capacity) >= 0)) {
    throw new Error("capacity is not a whole number");
  }
  // In plain sight in the person's home rather than under a data directory:
  // an agent's folder is where a person goes to see its work.
  let agentsDir = string("agents_dir");
  if (agentsDir === undefined) {
    if (!home) throw new Error("no agents_dir and no HOME to put ~/agents in");
    agentsDir = join(home, "agents");
  }
  if (!isAbsolute(agentsDir)) throw new Error(`agents_dir ${agentsDir} is not an absolute path`);
  const drainSeconds = file.drain_seconds ?? DRAIN_SECONDS;
  if (!(typeof drainSeconds === "number" && Number.isInteger(drainSeconds) && drainSeconds >= 0)) {
    throw new Error("drain_seconds is not a whole number");
  }
  const archiveDir = string("archive_dir");
  if (archiveDir !== undefined && !isAbsolute(archiveDir)) throw new Error(`archive_dir ${archiveDir} is not an absolute path`);
  let permissions = EVERY_TOOL;
  if (file.permissions !== undefined) {
    const p = file.permissions as Record<string, unknown>;
    const other = Object.keys(p).find((k) => k !== "allow" && k !== "deny");
    if (other) throw new Error(`unknown setting permissions.${other}`);
    permissions = {
      allow: strings(p.allow, "permissions.allow") ?? ["*"],
      deny: strings(p.deny, "permissions.deny") ?? [],
    };
  }
  return {
    serviceUrl: serviceUrl.replace(/\/+$/, ""),
    token,
    hostId,
    ...(capacity !== undefined ? { capacity: Number(capacity) } : {}),
    ...(file.labels !== undefined ? { labels: strings(file.labels, "labels") } : {}),
    agentsDir,
    drainSeconds,
    ...(archiveDir !== undefined ? { archiveDir } : {}),
    permissions,
  };
}
