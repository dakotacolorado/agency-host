#!/usr/bin/env node
import { readFileSync } from "node:fs";
import { loginDir } from "./claude.ts";
import { load } from "./config.ts";
import { Host } from "./host.ts";
import { hold } from "./lock.ts";
import { startSdkSession } from "./sdk.ts";
import { errorText, log } from "./sys.ts";

/** The release this is, as `--version` and each heartbeat say it: package.json's, beside `src/` and `dist/` alike. */
export const VERSION: string = (JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8")) as { version: string }).version;

const USAGE = `usage: agent-host [--config PATH]

  --config PATH   config file (default $XDG_CONFIG_HOME/agent-host/config.toml,
                  or ~/.config/agent-host/config.toml); AGENT_HOST_CONFIG also names it

Every setting is in the file: service_url, token, host_id (default the hostname),
capacity, labels, agents_dir (default ~/agents), [permissions] allow and deny
(default every tool allowed).`;

export function parseArgs(args: string[], env: NodeJS.ProcessEnv): { config: string | null } | { exit: number; say: string } {
  let config = env.AGENT_HOST_CONFIG || null;
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    if (a === "--config") {
      const path = args[++i];
      if (!path) return { exit: 2, say: "--config needs a path" };
      config = path;
    } else if (a === "--version") return { exit: 0, say: `agent-host ${VERSION}` };
    else if (a === "--help" || a === "-h") return { exit: 0, say: USAGE };
    else return { exit: 2, say: `unknown argument ${a}\n\n${USAGE}` };
  }
  return { config };
}

/** The environment each session starts from: the host's own, but for what the host sets for every session itself. */
function baseEnv(): { [k: string]: string } {
  const env: { [k: string]: string } = {};
  for (const [k, v] of Object.entries(process.env)) {
    if (v !== undefined && k !== "CLAUDE_CONFIG_DIR" && k !== "CLAUDE_SECURESTORAGE_CONFIG_DIR" && k !== "AGENT_HOST_CONFIG") env[k] = v;
  }
  return env;
}

async function main(): Promise<number | null> {
  const args = parseArgs(process.argv.slice(2), process.env);
  if ("exit" in args) {
    (args.exit ? process.stderr : process.stdout).write(`${args.say}\n`);
    return args.exit;
  }
  let config;
  try {
    config = load(args.config);
  } catch (e) {
    log(`agent-host: ${errorText(e)}`);
    return 2;
  }
  let lock;
  try {
    lock = await hold(config.agentsDir);
  } catch (e) {
    log(`agent-host: ${errorText(e)}`);
    return 3;
  }
  // A terminal closing is no reason to stop.
  process.on("SIGHUP", () => {});
  log(`agent-host ${VERSION}: host ${config.hostId} on ${config.serviceUrl}`);
  log(
    config.capacity !== undefined
      ? `will run sessions for ${config.capacity} agents, labelled ${(config.labels ?? []).join(", ")}`
      : "no capacity set: the service will put no agent here until one is",
  );
  log(`agents are in ${config.agentsDir}`);
  const host = new Host({
    config,
    version: VERSION,
    startSession: startSdkSession,
    login: loginDir(process.env.CLAUDE_CONFIG_DIR, config.agentsDir, log),
    baseEnv: baseEnv(),
  });
  host.start();
  let going = false;
  const go = (signal: string) => {
    if (going) return;
    going = true;
    log(`agent-host: ${signal}; its sessions are resumed when it starts again`);
    void host.stop().finally(() => {
      lock.close();
      process.exit(0);
    });
  };
  process.on("SIGTERM", () => go("SIGTERM"));
  process.on("SIGINT", () => go("SIGINT"));
  return null;
}

const code = await main();
if (code !== null) process.exit(code);
