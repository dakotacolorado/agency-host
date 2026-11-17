import { chmodSync, mkdirSync, readFileSync, symlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { Agents } from "./agents.ts";
import { EVERY_TOOL } from "./config.ts";
import { DEFAULT_LIMITS, envStream, limits, podmanArgs, releaseImage, run, type ContainerRun, type Containers } from "./container.ts";
import { write } from "./context.ts";
import { FakeSdk, scratch, settle } from "./testing.ts";

const AGENT = "3b0f6c1e-9a52-4c1d-8f43-2a6e1c7d9b10";
const S1 = "9d6a1f0e-4b7c-4e21-a3f8-0c5d2e7b4a19";
const S2 = "1d6a1f0e-4b7c-4e21-a3f8-0c5d2e7b4a20";
const C1 = "aaaaaaaa-0000-4000-8000-000000000001";

const CONTAINERS: Containers = { podman: "podman", image: "localhost/agency-agent:0.4.0-abc1234", slice: "agencyagents.slice", env: { PATH: "/usr/bin", HOME: "/home/me" } };

const box = (over: Partial<ContainerRun> = {}): ContainerRun => ({ ...CONTAINERS, name: `agent-${AGENT}`, dir: "/home/me/agents/ana", limits: { cpus: 2, memoryMiB: 4096 }, ...over });

const role = (settings: object) => ({
  key: "agent-roles/planner",
  version: 3,
  files: [
    { path: "skills/planner/SKILL.md", content: "plan", executable: false },
    { path: "skills/planner/role.json", content: JSON.stringify(settings), executable: false },
  ],
});

describe("a container's podman arguments", () => {
  it("mount the agent's folder at /agent with its limits, its own network and slice, and nothing of the host's home", () => {
    const args = podmanArgs(box(), ["claude", "--verbose"]);
    expect(args.slice(0, 3)).toEqual(["run", "--rm", "-i"]);
    const joined = args.join(" ");
    expect(joined).toContain(`--name agent-${AGENT}`);
    expect(joined).toContain("-v /home/me/agents/ana:/agent ");
    expect(joined).toContain("-w /agent ");
    expect(joined).toContain("--cpus 2 --memory 4096m");
    expect(args).toContain("--userns=keep-id");
    expect(args).toContain("--cgroup-parent=agencyagents.slice");
    expect(args.find((a) => a.startsWith("--network="))).toMatch(/^--network=pasta:/);
    expect(args.filter((a) => a === "-v")).toHaveLength(1);
    expect(joined).not.toContain("cellar");
    // The only host path mounted is the agent's own folder.
    expect(args.filter((a) => a.startsWith("/home/me"))).toEqual(["/home/me/agents/ana:/agent"]);
    expect(args).not.toContain("-e");
    expect(args.some((a) => a.startsWith("--env"))).toBe(false);
    expect(args.slice(-2)).toEqual(["claude", "--verbose"]);
    expect(args[args.indexOf("localhost/agency-agent:0.4.0-abc1234") + 1]).toBe("bash");
  });

  it("hold no /cellar mount, whether or not the host has /cellar", () => {
    expect(podmanArgs(box(), ["claude"]).filter((a) => a === "-v")).toHaveLength(1);
    expect(podmanArgs(box(), ["claude"]).join(" ")).not.toContain("/cellar");
  });
});

describe("a container's limits", () => {
  it("are its role's cpus and memoryMiB, each defaulting to 4 CPUs and 8 GiB", () => {
    const state = scratch("limits");
    expect(limits(state)).toEqual(DEFAULT_LIMITS);
    expect(DEFAULT_LIMITS).toEqual({ cpus: 4, memoryMiB: 8192 });
    write(state, undefined, [{ key: "workplace/global", version: 1, files: [{ path: "skills/global/role.json", content: '{"cpus":99}' }] }, role({ cpus: 1.5, memoryMiB: 2048 })]);
    expect(limits(state)).toEqual({ cpus: 1.5, memoryMiB: 2048 });
    write(state, undefined, [role({ cpus: 6 })]);
    expect(limits(state)).toEqual({ cpus: 6, memoryMiB: 8192 });
    write(state, undefined, [role({ cpus: -1, memoryMiB: "lots" })]);
    expect(limits(state)).toEqual(DEFAULT_LIMITS);
  });
});

describe("the release's image", () => {
  it("is tagged with the release folder this host runs from, and none outside one", () => {
    const base = scratch("release");
    mkdirSync(join(base, "releases/0.4.0-abc1234/src"), { recursive: true });
    symlinkSync(join(base, "releases/0.4.0-abc1234"), join(base, "release"));
    expect(releaseImage(join(base, "release/src/"))).toBe("localhost/agency-agent:0.4.0-abc1234");
    expect(releaseImage(new URL(".", import.meta.url).pathname)).toBeNull();
  });
});

describe("a container's environment", () => {
  it("goes down fd 3, every value with it, and none on podman's command line", async () => {
    const dir = scratch("podman");
    // A stand-in podman: what it was run with, and what came down fd 3.
    const podman = join(dir, "podman");
    writeFileSync(podman, `#!/bin/sh\nprintf '%s\\n' "$@" > ${dir}/args\ncat <&3 > ${dir}/fd3\necho said\necho oops >&2\n`);
    chmodSync(podman, 0o755);
    const env = { CLAUDE_CONFIG_DIR: "/agent/home/.claude", GH_TOKEN: "very-secret-value", MULTI: "two\nlines" };
    const errors: string[] = [];
    const child = run(box({ podman }), ["claude", "--verbose"], env, undefined, (t) => errors.push(t));
    let out = "";
    child.stdout!.on("data", (d: Buffer) => (out += d.toString()));
    const code = await new Promise((r) => child.on("exit", r));
    await settle();
    expect(code).toBe(0);
    expect(out).toBe("said\n");
    expect(errors.join("")).toBe("oops\n");
    expect(readFileSync(join(dir, "fd3"), "utf8")).toBe(envStream(env));
    expect(envStream(env)).toBe("CLAUDE_CONFIG_DIR=/agent/home/.claude\0GH_TOKEN=very-secret-value\0MULTI=two\nlines\0");
    const args = readFileSync(join(dir, "args"), "utf8");
    expect(args).not.toContain("very-secret-value");
    expect(args).not.toContain("GH_TOKEN");
    expect(args).toContain("--preserve-fds=1");
  });

  it("is read into the container's own by a prelude, as bash does it", async () => {
    const dir = scratch("prelude");
    // podman run's own arguments dropped: what follows the image is run here, fd 3 and all.
    const podman = join(dir, "podman");
    writeFileSync(podman, `#!/bin/bash\nwhile [ "$1" != "${CONTAINERS.image}" ]; do shift; done; shift\nexec "$@"\n`);
    chmodSync(podman, 0o755);
    const child = run(box({ podman }), ["sh", "-c", 'printf "%s|%s|%s" "$GH_TOKEN" "$MULTI" "$(ls /proc/self/fd | wc -l)"'], { GH_TOKEN: "t0k", MULTI: "a\nb" }, undefined, () => {});
    let out = "";
    child.stdout!.on("data", (d: Buffer) => (out += d.toString()));
    await new Promise((r) => child.on("exit", r));
    // fd 3 is closed before Claude Code runs: 0, 1, 2 and ls's own.
    expect(out).toBe("t0k|a\nb|4");
  });

  it("stops podman when the SDK aborts it", async () => {
    const dir = scratch("abort");
    const podman = join(dir, "podman");
    writeFileSync(podman, "#!/bin/sh\nexec sleep 30\n");
    chmodSync(podman, 0o755);
    const abort = new AbortController();
    const child = run(box({ podman }), ["claude"], {}, abort.signal, () => {});
    const exited = new Promise((r) => child.on("exit", (_c, s) => r(s)));
    abort.abort();
    expect(await exited).toBe("SIGTERM");
  });
});

function host(dir = scratch("agents"), containers: Containers | null = CONTAINERS) {
  const sdk = new FakeSdk();
  const agents = new Agents({ dir, startSession: sdk.start, permissions: EVERY_TOOL, login: null, baseEnv: { PATH: "/usr/bin", HOME: "/home/me" }, changed: () => {}, ...(containers ? { containers } : {}) });
  return { dir, sdk, agents };
}

const SERVERS = { tracker: { type: "http", url: "${WT_URL}/mcp", headers: { Authorization: "Bearer ${WT_TOKEN}" } } };

describe("a container agent", () => {
  it("runs its session in its container, seeing its folder at /agent, given its own variables and none of the host's", () => {
    const { dir, sdk, agents } = host();
    agents.create(AGENT, "ana");
    agents.start({ commandId: C1, agentId: AGENT, sessionId: S1, resume: false, runtime: "container", text: "go", documents: [role({ cpus: 2, memoryMiB: 3072 })], env: { WT_URL: "https://wt" }, secrets: { WT_TOKEN: "s3cret" }, mcpServers: SERVERS });
    const o = sdk.last.options;
    expect(o.cwd).toBe("/agent");
    expect(o.container).toEqual({ ...CONTAINERS, name: `agent-${AGENT}`, dir: join(dir, "ana"), limits: { cpus: 2, memoryMiB: 3072 } });
    expect(o.env).toEqual({ WT_URL: "https://wt", WT_TOKEN: "s3cret", HOME: "/agent/home", CLAUDE_CONFIG_DIR: "/agent/home/.claude" });
    // Its Claude Code fills them in from its environment: no token on the command line the SDK makes of them.
    expect(o.mcpServers).toEqual(SERVERS);
    const state = join(dir, "ana/home/.claude");
    expect(readFileSync(join(state, "CLAUDE.md"), "utf8")).toContain("You are agent `ana`, and your folder is `/agent`.");
    expect(JSON.parse(readFileSync(join(state, ".claude.json"), "utf8")).projects["/agent"]).toEqual({ hasTrustDialogAccepted: true });
    expect(agents.all()[0]!.home.latest()).toMatchObject({ sessionId: S1, runtime: "container" });
  });

  it("is resumed in its container: by a start, after a drain, and by a host that restarted", async () => {
    const first = host();
    first.agents.create(AGENT, "ana");
    first.agents.start({ agentId: AGENT, sessionId: S1, resume: false, runtime: "container", text: "go", secrets: { GH_TOKEN: "t" } });
    await settle();
    await first.agents.stop(AGENT);
    // A start that does not say where carries on where the last session ran.
    first.agents.start({ agentId: AGENT, sessionId: S2, resume: true, secrets: { GH_TOKEN: "t" } });
    expect(first.sdk.last.options).toMatchObject({ resume: S1, cwd: "/agent", container: { name: `agent-${AGENT}` } });
    await settle();
    await first.agents.drain();
    first.agents.message(AGENT, C1, "more");
    expect(first.sdk.sessions).toHaveLength(3);
    expect(first.sdk.last.options).toMatchObject({ resume: S1, cwd: "/agent", container: { name: `agent-${AGENT}` } });

    const state = join(first.dir, "ana/home/.claude");
    mkdirSync(join(state, "projects/-agent"), { recursive: true });
    writeFileSync(join(state, "projects/-agent", `${S1}.jsonl`), "{}\n");
    const again = host(first.dir);
    again.agents.reconcile();
    expect(again.sdk.sessions).toHaveLength(0);
    again.agents.start({ agentId: AGENT, sessionId: "s3", resume: true, runtime: "container", secrets: { GH_TOKEN: "t" } });
    expect(again.sdk.last.options).toMatchObject({ resume: S1, cwd: "/agent", env: { GH_TOKEN: "t", HOME: "/agent/home" }, container: { dir: join(first.dir, "ana") } });
  });

  it("is refused by a host with no agent image, which runs nothing", () => {
    const { sdk, agents } = host(scratch("agents"), null);
    agents.create(AGENT, "ana");
    expect(() => agents.start({ agentId: AGENT, sessionId: S1, resume: false, runtime: "container" })).toThrow(/no agent image/);
    expect(sdk.sessions).toHaveLength(0);
    expect(agents.all()[0]!.home.latest()).toBeNull();
  });
});

describe("a host agent", () => {
  it("runs on the host as before, whether its start says host or nothing", () => {
    for (const runtime of ["host", undefined] as const) {
      const { dir, sdk, agents } = host();
      agents.create(AGENT, "ana");
      agents.start({ agentId: AGENT, sessionId: S1, resume: false, ...(runtime ? { runtime } : {}), secrets: { WT_TOKEN: "s3cret" }, env: { WT_URL: "https://wt" }, mcpServers: SERVERS });
      const o = sdk.last.options;
      expect(o.container).toBeUndefined();
      expect(o.cwd).toBe(join(dir, "ana"));
      expect(o.env).toMatchObject({ PATH: "/usr/bin", HOME: "/home/me", CLAUDE_CONFIG_DIR: join(dir, "ana/home/.claude") });
      expect(o.mcpServers).toEqual({ tracker: { type: "http", url: "https://wt/mcp", headers: { Authorization: "Bearer s3cret" } } });
      expect(agents.all()[0]!.home.latest()!.runtime).toBeUndefined();
      expect(readFileSync(join(dir, "ana/home/.claude/CLAUDE.md"), "utf8")).toContain(`your folder is \`${join(dir, "ana")}\``);
    }
  });
});
