import { writeFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { hold, lockPath } from "./lock.ts";
import { scratch } from "./testing.ts";

describe("the lock", () => {
  it("lets one host hold an agents folder, and takes over one a host that died left", async () => {
    const dir = scratch("lock");
    const first = await hold(dir);
    await expect(hold(dir)).rejects.toThrow(/another agency-host \(or agent-host\) is already running/);
    await new Promise((r) => first.close(r));
    // As a machine unplugged leaves it.
    writeFileSync(lockPath(dir), "");
    const again = await hold(dir);
    await new Promise((r) => again.close(r));
  });

  it("keeps a lock path short enough for a unix socket", () => {
    expect(lockPath("/srv/agents")).toBe("/srv/agents/.agent-host.sock");
    expect(Buffer.byteLength(lockPath(`/${"a".repeat(200)}`))).toBeLessThanOrEqual(100);
  });
});
