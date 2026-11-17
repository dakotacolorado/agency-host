#!/usr/bin/env node
import { readFileSync } from "node:fs";
import { loginDir } from "./claude.ts";
import { banner, wantsColor } from "./banner.ts";
import { load } from "./config.ts";
import { releaseImage, SLICE, type Containers } from "./container.ts";
import { Host } from "./host.ts";
import { hold } from "./lock.ts";
import { startSdkSession } from "./sdk.ts";
import { errorText, log } from "./sys.ts";

/** The release this is, as `--version` and each heartbeat say it: package.json's, beside `src/` and `dist/` alike. */
export const VERSION: string = (JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8")) as { version: string }).version;

const USAGE = `usage: agency-host [--config PATH]

  --config PATH   config file (default $XDG_CONFIG_HOME/agency-host/config.toml,
                  or ~/.config/agency-host/config.toml); AGENCY_HOST_CONFIG also names it

Every setting is in the file: service_url, token, host_id (default the hostname),
capacity, labels, agents_dir (default ~/agents), [permissions] allow and deny
(default every tool allowed), drain_seconds (default 1800).

A session whose agent runs in a container runs in AGENCY_HOST_IMAGE (default
localhost/agency-agent:<release>, from releases/<release>/), under the slice
AGENCY_HOST_SLICE (default agencyagents.slice).

SIGTERM or SIGINT stops it now: its sessions are resumed when it starts again,
the turn each was in cut off. SIGUSR2 drains it: each session is closed once its
turn is over, and it exits once none is live, or at drain_seconds.`;

/** Read by agency-host and by its releases as agent-host, and never given to a session. */
const CONFIG_VARS = ["AGENCY_HOST_CONFIG", "AGENT_HOST_CONFIG", "AGENCY_HOST_IMAGE", "AGENCY_HOST_SLICE"];

/**
 * How container sessions run: in AGENCY_HOST_IMAGE, or the image of the
 * release this is (`localhost/agency-agent:<release>`), under
 * AGENCY_HOST_SLICE or agencyagents.slice. With neither image, none run here.
 */
function containers(env: { [k: string]: string }): Containers | undefined {
  const image = process.env.AGENCY_HOST_IMAGE || releaseImage(new URL(".", import.meta.url).pathname);
  if (!image) return undefined;
  return { podman: "podman", image, slice: process.env.AGENCY_HOST_SLICE || SLICE, env };
}

export function parseArgs(args: string[], env: NodeJS.ProcessEnv, color = false): { config: string | null } | { exit: number; say: string } {
  let config = env.AGENCY_HOST_CONFIG || env.AGENT_HOST_CONFIG || null;
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    if (a === "--config") {
      const path = args[++i];
      if (!path) return { exit: 2, say: "--config needs a path" };
      config = path;
    } else if (a === "--version") return { exit: 0, say: `${banner(VERSION, color)}\n\nagency-host ${VERSION}` };
    else if (a === "--help" || a === "-h") return { exit: 0, say: `${banner(VERSION, color)}\n\n${USAGE}` };
    else return { exit: 2, say: `unknown argument ${a}\n\n${USAGE}` };
  }
  return { config };
}

/** The environment each session starts from: the host's own, but for what the host sets for every session itself. */
function baseEnv(): { [k: string]: string } {
  const env: { [k: string]: string } = {};
  for (const [k, v] of Object.entries(process.env)) {
    if (v !== undefined && k !== "CLAUDE_CONFIG_DIR" && k !== "CLAUDE_SECURESTORAGE_CONFIG_DIR" && !CONFIG_VARS.includes(k)) env[k] = v;
  }
  return env;
}

async function main(): Promise<number | null> {
  const args = parseArgs(process.argv.slice(2), process.env, wantsColor(process.stdout));
  if ("exit" in args) {
    (args.exit ? process.stderr : process.stdout).write(`${args.say}\n`);
    return args.exit;
  }
  let config;
  try {
    config = load(args.config);
  } catch (e) {
    log(`agency-host: ${errorText(e)}`);
    return 2;
  }
  let lock;
  try {
    lock = await hold(config.agentsDir);
  } catch (e) {
    log(`agency-host: ${errorText(e)}`);
    return 3;
  }
  // A terminal closing is no reason to stop.
  process.on("SIGHUP", () => {});
  log(`agency-host ${VERSION}: host ${config.hostId} on ${config.serviceUrl}`);
  log(
    config.capacity !== undefined
      ? `will run sessions for ${config.capacity} agents, labelled ${(config.labels ?? []).join(", ")}`
      : "no capacity set: the service will put no agent here until one is",
  );
  log(`agents are in ${config.agentsDir}`);
  const env = baseEnv();
  const boxes = containers(env);
  log(boxes ? `container sessions run in ${boxes.image}, in ${boxes.slice}` : "no agent image: no session runs in a container here");
  const host = new Host({
    config,
    version: VERSION,
    startSession: startSdkSession,
    login: loginDir(process.env.CLAUDE_CONFIG_DIR, config.agentsDir, log),
    baseEnv: env,
    ...(boxes ? { containers: boxes } : {}),
  });
  let stopping = false;
  let draining = false;
  const exit = () => {
    lock.close();
    process.exit(0);
  };
  const go = (signal: string) => {
    if (stopping) return;
    stopping = true;
    log(`agency-host: ${signal}; its sessions are resumed when it starts again`);
    void host.stop().finally(exit);
  };
  process.on("SIGTERM", () => go("SIGTERM"));
  process.on("SIGINT", () => go("SIGINT"));
  // Installed at the start, as a release's deploy reads the signals caught to know a host can drain. Not SIGUSR1, which is Node's inspector's.
  process.on("SIGUSR2", () => {
    if (stopping || draining) return;
    draining = true;
    log(`agency-host: SIGUSR2; draining: each session is closed as its turn ends, and the host exits once none is live, or in ${config.drainSeconds}s`);
    void host.drain().finally(exit);
  });
  host.start();
  return null;
}

const code = await main();
if (code !== null) process.exit(code);
