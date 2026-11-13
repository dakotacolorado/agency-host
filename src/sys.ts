import { hostname as osHostname } from "node:os";

/** Now, as the service takes an instant: UTC to the millisecond. */
export function now(): string {
  return new Date().toISOString();
}

/** Seconds since the epoch as an instant, or null for one that is not a time. */
export function epoch(secs: number): string | null {
  if (!Number.isFinite(secs) || secs < 0) return null;
  const d = new Date(secs * 1000);
  return Number.isNaN(d.getTime()) ? null : d.toISOString();
}

/** Whether `at` is an instant the service takes: UTC, as Claude Code writes it. One it cannot take refuses the whole report. */
export function isInstant(at: unknown): at is string {
  return typeof at === "string" && /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z$/.test(at);
}

/** The machine's name up to its first dot. */
export function hostname(): string | null {
  const name = osHostname().split(".")[0] ?? "";
  return name || null;
}

export function log(message: string): void {
  process.stderr.write(`${message}\n`);
}

export function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
