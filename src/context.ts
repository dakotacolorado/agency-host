/**
 * What a start carries for the agent beyond its first message: files written
 * into its own Claude Code folder before it starts, the variables and tokens
 * its session is given, and the MCP servers it may call. The host knows
 * nothing of where any of it comes from; it only writes it.
 *
 * A start's `context` (agent-service's own clients) and `documents` (honed
 * skills) write only files under `skills/`. What they wrote is listed in
 * `.context.json`, the context's at the top and the documents' under
 * `documents`, so the next start first removes it.
 */
import { chmodSync, existsSync, mkdirSync, readFileSync, rmdirSync, unlinkSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

export interface ContextFile {
  path: string;
  content: string;
  executable?: boolean;
}

export interface Context {
  key: string;
  version: number;
  files: ContextFile[];
}

/** What `.context.json` holds. The context's fields come first, so a script that reads the first `key` and `version` reads the context's. */
export interface Written {
  key: string;
  version: number;
  paths: string[];
  documents?: Written[];
}

const SKILLS = "skills";
const MANIFEST = ".context.json";
/** Set by the host for every session, so a start may not set them. */
const RESERVED = new Set(["CLAUDE_CONFIG_DIR", "CLAUDE_SECURESTORAGE_CONFIG_DIR"]);

/** A plain relative path under `skills/`, read as written: a `.` or `..` segment is refused, not resolved. */
function allowed(path: string): boolean {
  const parts = path.split("/");
  return parts.length > 1 && parts[0] === SKILLS && parts.every((p) => p !== "" && p !== "." && p !== "..");
}

export function written(state: string): Written | null {
  try {
    return JSON.parse(readFileSync(join(state, MANIFEST), "utf8")) as Written;
  } catch {
    return null;
  }
}

/**
 * Writes `context`, then `documents`, into `state`, an agent's Claude Code
 * folder, in place of what the last start wrote: every path is checked
 * before anything is touched, then the last start's files are removed and
 * these written.
 */
export function write(state: string, context: Context | undefined, documents: Context[]): Written {
  const all = [...(context ? [context] : []), ...documents];
  const seen = new Set<string>();
  for (const c of all) {
    const bad = c.files.find((f) => !allowed(f.path));
    if (bad) throw new Error(`${c.key} may write only files under ${SKILLS}/, not ${JSON.stringify(bad.path)}`);
    const twice = c.files.find((f) => seen.has(f.path) || !seen.add(f.path));
    if (twice) throw new Error(`${c.key} writes ${twice.path}, as one before it does`);
  }
  const last = written(state);
  if (last) {
    for (const path of [last, ...(last.documents ?? [])].flatMap((w) => w.paths ?? [])) {
      // A manifest is the host's own, but is read back as untrusted all the same.
      if (!allowed(path)) continue;
      const file = join(state, path);
      if (existsSync(file)) {
        unlinkSync(file);
        prune(state, file);
      }
    }
  }
  const done: Written = context ? put(state, context) : { key: "", version: 0, paths: [] };
  const docs = documents.map((d) => put(state, d));
  if (docs.length) done.documents = docs;
  mkdirSync(state, { recursive: true });
  writeFileSync(join(state, MANIFEST), `${JSON.stringify(done, null, 2)}\n`);
  return done;
}

function put(state: string, context: Context): Written {
  const paths: string[] = [];
  for (const f of context.files) {
    const file = join(state, f.path);
    mkdirSync(dirname(file), { recursive: true });
    writeFileSync(file, f.content);
    chmodSync(file, f.executable ? 0o755 : 0o644);
    paths.push(f.path);
  }
  return { key: context.key, version: context.version, paths };
}

/** Removes the folders a removed file leaves empty, up to `state`. */
function prune(state: string, file: string): void {
  for (let dir = dirname(file); dir !== state && dir.startsWith(state); dir = dirname(dir)) {
    try {
      rmdirSync(dir);
    } catch {
      return;
    }
  }
}

export function variable(name: string): boolean {
  return /^[A-Za-z_][A-Za-z0-9_]*$/.test(name);
}

/** The variables a start sets, but for those the host sets itself. */
export function env(given: Record<string, string> | undefined): Record<string, string> {
  return Object.fromEntries(Object.entries(given ?? {}).filter(([k]) => !RESERVED.has(k)));
}

/**
 * Writes `variables` to `file` as shell assignments, readable by the host's
 * user alone; with none, removes the last. A session resumed after the host
 * restarts is given them again from here.
 */
export function writeCredentials(file: string, variables: Record<string, string>): void {
  const entries = Object.entries(variables);
  if (!entries.length) {
    if (existsSync(file)) unlinkSync(file);
    return;
  }
  const bad = entries.find(([k]) => !variable(k));
  if (bad) throw new Error(`${JSON.stringify(bad[0])} is not a variable a session can be given`);
  const text = entries.map(([k, v]) => `${k}='${v.replaceAll("'", `'\\''`)}'\n`).join("");
  // Made, or made again, readable by its owner alone before anything is in it.
  if (existsSync(file)) chmodSync(file, 0o600);
  writeFileSync(file, text, { mode: 0o600 });
  chmodSync(file, 0o600);
}

/** What `writeCredentials` wrote, read back. */
export function readCredentials(file: string): Record<string, string> {
  let text: string;
  try {
    text = readFileSync(file, "utf8");
  } catch {
    return {};
  }
  const out: Record<string, string> = {};
  // A value may hold newlines, and a quote only as `'\''`.
  for (const m of text.matchAll(/^([A-Za-z_][A-Za-z0-9_]*)='((?:[^']|'\\'')*)'$/gm)) out[m[1]!] = m[2]!.replaceAll(`'\\''`, "'");
  return out;
}

export type McpServers = Record<string, unknown>;

/**
 * `servers` with each `${VAR}` (or `${VAR:-default}`) in its strings filled
 * from `env`, as Claude Code fills an `.mcp.json`. A reference with no value
 * and no default is left as written and named in `missing`, so a server
 * given one fails visibly rather than with an empty token.
 */
export function expand(servers: McpServers, env: Record<string, string | undefined>): { servers: McpServers; missing: string[] } {
  const missing = new Set<string>();
  const fill = (v: unknown): unknown => {
    if (typeof v === "string") {
      return v.replace(/\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}/g, (whole, name: string, fallback: string | undefined) => {
        const value = env[name];
        if (value !== undefined) return value;
        if (fallback !== undefined) return fallback;
        missing.add(name);
        return whole;
      });
    }
    if (Array.isArray(v)) return v.map(fill);
    if (v && typeof v === "object") return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, fill(x)]));
    return v;
  };
  return { servers: fill(servers) as McpServers, missing: [...missing] };
}
