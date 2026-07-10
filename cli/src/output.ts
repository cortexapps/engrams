/**
 * Output helpers. The contract carried over from the Rust engram-cli so the
 * jq pipelines in deploy/dev scripts port mechanically:
 *   - default output is a human-readable table / key-value view
 *   - --json prints pretty JSON to stdout and NOTHING else
 *   - errors go to stderr prefixed `engrams:` and exit 1
 */

import { ConnectError } from "@connectrpc/connect";

export function printJson(value: unknown): void {
  console.log(JSON.stringify(value, null, 2));
}

export function fail(message: string): never {
  console.error(`engrams: ${message}`);
  process.exit(1);
}

/** Render an RPC/transport failure and exit — the one error edge for verbs. */
export function failWith(err: unknown): never {
  if (err instanceof ConnectError) {
    // rawMessage strips the "[code] " prefix Connect prepends to message.
    fail(`${ConnectError.from(err).code}: ${err.rawMessage}`);
  }
  fail(err instanceof Error ? err.message : String(err));
}

export function truncate(s: string, max: number): string {
  return [...s].length <= max ? s : `${[...s].slice(0, Math.max(0, max - 1)).join("")}…`;
}

/** Left-aligned fixed-width columns, like the Rust CLI's format strings. */
export function table(header: string[], rows: string[][], widths: number[]): void {
  const line = (cells: string[]) =>
    cells
      .map((c, i) => (i === cells.length - 1 ? c : truncate(c, widths[i]!).padEnd(widths[i]!)))
      .join("  ");
  console.log(line(header));
  for (const row of rows) console.log(line(row));
}

/** `key             : value` detail view, matching the Rust CLI's gets. */
export function detail(pairs: Array<[string, string | undefined]>): void {
  for (const [k, v] of pairs) {
    if (v !== undefined && v !== "") console.log(`${k.padEnd(16)}: ${v}`);
  }
}
