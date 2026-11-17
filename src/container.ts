/**
 * Claude Code in a container, for an agent whose runtime is `container`: the
 * Agent SDK and the socket stay on the host, and the SDK's Claude Code
 * process is `podman run` of the agent image (`image/Containerfile`), with
 * the agent's folder at /agent, a network of its own
 * and its role's CPU and memory, and nothing else of the host's.
 *
 * Its environment, secrets among them, goes down an inherited pipe (fd 3)
 * that a prelude in the container reads into its own before it runs Claude
 * Code. Not `-e NAME=value`, which every process on the host can read in
 * its command line; nor `-e NAME`, which podman writes, value and all, into
 * the container's config under its storage and shows in `podman inspect`.
 */
import { spawn, type ChildProcess } from "node:child_process";
import { readFileSync, realpathSync } from "node:fs";
import { basename, dirname, join } from "node:path";
import { written } from "./context.ts";

/** Where the agent's folder, its home and its Claude Code are, inside. */
export const INSIDE = "/agent";
export const INSIDE_HOME = `${INSIDE}/home`;
export const INSIDE_STATE = `${INSIDE_HOME}/.claude`;

/** The image a release builds (release-routines' agency-host `agent_image`), tagged with the release. */
export const IMAGE = "localhost/agency-agent";
/** The agents' containers' slice. No `-` in it: systemd reads one as nesting. */
export const SLICE = "agencyagents.slice";

export interface Limits {
  cpus: number;
  memoryMiB: number;
}

/** What a role whose `role.json` says nothing of it is given. */
export const DEFAULT_LIMITS: Limits = { cpus: 4, memoryMiB: 8192 };

/**
 * pasta would hand the container the host's upstream (LAN) resolver, which
 * knows no tailnet names, and quad100 answers a container only for those.
 * So the container asks this address, which pasta forwards to the host's own
 * resolver: systemd-resolved, which splits them as the host does.
 */
const DNS = "169.254.1.1";

/** What the host runs containers with. */
export interface Containers {
  podman: string;
  image: string;
  slice: string;
  /** podman's own environment: the host's. */
  env: Record<string, string>;
}

/** One agent's container. */
export interface ContainerRun extends Containers {
  name: string;
  /** The agent's folder on the host, mounted at /agent. */
  dir: string;
  limits: Limits;
}

/** A release's image, when this host runs from a release folder (`releases/<tag>/src/`); null otherwise. */
export function releaseImage(src: string): string | null {
  let root: string;
  try {
    root = dirname(realpathSync(src));
  } catch {
    return null;
  }
  return basename(dirname(root)) === "releases" ? `${IMAGE}:${basename(root)}` : null;
}

const positive = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v) && v > 0;

/**
 * The CPUs and memory the agent's role gives it: `cpus` and `memoryMiB` in
 * the `role.json` of the role skill its last start wrote into `state`
 * (`agent-roles/<role>`), each defaulting on its own.
 */
export function limits(state: string): Limits {
  const role = written(state)?.documents?.find((d) => d.key.startsWith("agent-roles/"));
  const path = role?.paths?.find((p) => /^skills\/[^/]+\/role\.json$/.test(p));
  if (!path) return DEFAULT_LIMITS;
  let settings: { cpus?: unknown; memoryMiB?: unknown };
  try {
    settings = JSON.parse(readFileSync(join(state, path), "utf8")) as typeof settings;
  } catch {
    return DEFAULT_LIMITS;
  }
  return {
    cpus: positive(settings.cpus) ? settings.cpus : DEFAULT_LIMITS.cpus,
    memoryMiB: positive(settings.memoryMiB) && Number.isInteger(settings.memoryMiB) ? settings.memoryMiB : DEFAULT_LIMITS.memoryMiB,
  };
}

/** Reads `NAME=value` pairs, each ended by a NUL, from fd 3 into the environment, closes it, and runs what follows. */
const PRELUDE = 'while IFS= read -r -d "" kv; do export "$kv"; done <&3; exec 3<&-; exec "$@"';

/** `podman run`'s arguments for `command` in the agent's container: no value of its environment among them. */
export function podmanArgs(r: ContainerRun, command: string[]): string[] {
  return [
    "run",
    "--rm",
    "-i",
    // A container a host that died left behind, still holding the name, is taken over.
    "--replace",
    "--name",
    r.name,
    "--userns=keep-id",
    `--network=pasta:--dns-forward,${DNS}`,
    "--dns",
    DNS,
    `--cgroup-parent=${r.slice}`,
    "--cpus",
    String(r.limits.cpus),
    "--memory",
    `${r.limits.memoryMiB}m`,
    "--preserve-fds=1",
    "-v",
    `${r.dir}:${INSIDE}`,
    "-w",
    INSIDE,
    r.image,
    "bash",
    "-c",
    PRELUDE,
    "agency-host",
    ...command,
  ];
}

/** What fd 3 carries: each variable as `NAME=value` and a NUL. */
export function envStream(env: Record<string, string | undefined>): string {
  return Object.entries(env)
    .filter(([k, v]) => v !== undefined && /^[A-Za-z_][A-Za-z0-9_]*$/.test(k) && !v.includes("\0"))
    .map(([k, v]) => `${k}=${v}\0`)
    .join("");
}

/**
 * Runs `command` in the agent's container with `env` as its whole
 * environment, as the Agent SDK's `spawnClaudeCodeProcess` would run it on
 * the host; its stderr goes to `stderr`. Aborted, podman is stopped, which
 * stops the container it runs and removes it.
 */
export function run(r: ContainerRun, command: string[], env: Record<string, string | undefined>, signal: AbortSignal | undefined, stderr: (text: string) => void): ChildProcess {
  const child = spawn(r.podman, podmanArgs(r, command), { env: r.env, stdio: ["pipe", "pipe", "pipe", "pipe"] });
  const pipe = child.stdio[3] as NodeJS.WritableStream & { on(e: "error", f: () => void): void };
  // Gone already when podman fails before it runs anything: that failure is on its stderr.
  pipe.on("error", () => {});
  pipe.end(envStream(env));
  child.stderr!.on("data", (d: Buffer) => stderr(d.toString()));
  if (signal) {
    const abort = () => child.kill("SIGTERM");
    if (signal.aborted) abort();
    else signal.addEventListener("abort", abort, { once: true });
  }
  return child;
}
