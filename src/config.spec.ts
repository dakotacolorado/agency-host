import { describe, expect, it } from "vitest";
import { allows, EVERY_TOOL, fromText, load } from "./config.ts";

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
      permissions: EVERY_TOOL,
    });
  });

  it("leaves capacity and labels unset rather than guessed, and accepts a setting an older release read", () => {
    const config = read(BASE);
    expect(config.capacity).toBeUndefined();
    expect(config.labels).toBeUndefined();
    expect(read(`${BASE}tmux_socket = "beta"`)).toEqual(config);
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
    expect(() => load("/nonexistent/agent-host.toml")).toThrow(/\/nonexistent\/agent-host.toml/);
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
