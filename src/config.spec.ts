import { describe, expect, it } from "vitest";
import { allows, defaultPath, EVERY_TOOL, fromText, load } from "./config.ts";

const BASE = 'service_url = "https://x"\ntoken = "t"\nhost_id = "a"\n';
const read = (text: string) => fromText(text, "/home/me");

describe("the config file", () => {
  it("reads every setting", () => {
    expect(read('service_url = "https://beta.example/"\ntoken = "t"\nhost_id = "studio"\ncapacity = 2\nlabels = ["macos", "linux"]')).toEqual({
      serviceUrl: "https://beta.example",
      token: "t",
      hostId: "studio",
      capacity: 2,
      labels: ["macos", "linux"],
      agentsDir: "/home/me/agents",
      drainSeconds: 1800,
      permissions: EVERY_TOOL,
    });
  });

  it("waits drain_seconds for a drain, 1800 unless it says otherwise", () => {
    expect(read(BASE).drainSeconds).toBe(1800);
    expect(read(`${BASE}drain_seconds = 60`).drainSeconds).toBe(60);
    expect(() => read(`${BASE}drain_seconds = -1`)).toThrow(/drain_seconds is not a whole number/);
    expect(() => read(`${BASE}drain_seconds = "60"`)).toThrow(/drain_seconds/);
  });

  it("leaves capacity and labels unset rather than guessed, and accepts a setting an older release read", () => {
    const config = read(BASE);
    expect(config.capacity).toBeUndefined();
    expect(config.labels).toBeUndefined();
    expect(read(`${BASE}tmux_socket = "beta"`)).toEqual(config);
  });

  it("archives deleted agents only where archive_dir says, at an absolute path", () => {
    expect(read(BASE).archiveDir).toBeUndefined();
    expect(read(`${BASE}archive_dir = "/cellar/agency/agents"`).archiveDir).toBe("/cellar/agency/agents");
    expect(() => read(`${BASE}archive_dir = "agents"`)).toThrow(/archive_dir agents is not an absolute path/);
  });

  it("puts agents in ~/agents unless it says otherwise, and only at an absolute path", () => {
    expect(read(BASE).agentsDir).toBe("/home/me/agents");
    expect(read(`${BASE}agents_dir = "/srv/agents"`).agentsDir).toBe("/srv/agents");
    expect(() => read(`${BASE}agents_dir = "agents"`)).toThrow(/not an absolute path/);
  });

  it("names what is missing or unknown", () => {
    expect(() => read('service_url = "https://x"')).toThrow(/no token/);
    expect(() => read('service = "https://x"')).toThrow(/unknown setting service/);
    expect(() => read(`${BASE}[permissions]\nmaybe = ["x"]`)).toThrow(/permissions.maybe/);
    expect(() => load("/nonexistent/agency-host.toml")).toThrow(/\/nonexistent\/agency-host.toml/);
  });
});

describe("the config file's default path", () => {
  const env = { HOME: "/home/me" };
  it("is agency-host/config.toml in the config folder", () => {
    expect(defaultPath(env, () => false)).toBe("/home/me/.config/agency-host/config.toml");
    expect(defaultPath({ XDG_CONFIG_HOME: "/xdg" }, () => false)).toBe("/xdg/agency-host/config.toml");
  });

  it("is agent-host/config.toml when only that exists, as agent-host left it", () => {
    const only = (path: string) => (p: string) => p === path;
    expect(defaultPath(env, only("/home/me/.config/agent-host/config.toml"))).toBe("/home/me/.config/agent-host/config.toml");
    expect(defaultPath(env, () => true)).toBe("/home/me/.config/agency-host/config.toml");
  });
});

describe("permissions", () => {
  it("allows every tool unless the file denies it or allows only some", () => {
    const all = read(BASE).permissions;
    expect(allows(all, "Bash") && allows(all, "mcp__github__create_pr")).toBe(true);
    const some = read(`${BASE}[permissions]\nallow = ["Read", "mcp__github__*"]\ndeny = ["mcp__github__merge*"]`).permissions;
    expect(allows(some, "Read") && allows(some, "mcp__github__create_pr")).toBe(true);
    expect(allows(some, "Bash")).toBe(false);
    expect(allows(some, "mcp__github__merge_pr")).toBe(false);
    expect(allows(some, "Rea")).toBe(false);
    const denied = read(`${BASE}[permissions]\ndeny = ["WebFetch"]`).permissions;
    expect(allows(denied, "Bash") && !allows(denied, "WebFetch")).toBe(true);
  });
});
