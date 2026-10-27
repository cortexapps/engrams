/** An in-memory `SessionRelayStore` with the real CAS semantics, for tests
 * that install, render and close session relays without Postgres. */

import type { SessionRelayRow, SessionRelayStore } from "../../../db/session-relays.ts";

export interface FakeRelayStore extends SessionRelayStore {
  rows: Map<string, SessionRelayRow>;
  /** Every mutation, in order: `install`, `repoint`, `render`, `remove`. */
  writes: string[];
}

export function makeFakeRelayStore(): FakeRelayStore {
  const rows = new Map<string, SessionRelayRow>();
  const writes: string[] = [];
  const clone = (row: SessionRelayRow): SessionRelayRow => structuredClone(row);
  return {
    rows,
    writes,
    async get(sessionId) {
      const row = rows.get(sessionId);
      return row ? clone(row) : null;
    },
    async install(input) {
      const existing = rows.get(input.sessionId);
      if (existing) return { row: clone(existing), created: false };
      const row: SessionRelayRow = { ...input, version: 1 };
      rows.set(input.sessionId, clone(row));
      writes.push("install");
      return { row: clone(row), created: true };
    },
    async update(sessionId, expectVersion, patch) {
      const row = rows.get(sessionId);
      if (!row || row.version !== expectVersion) return false;
      rows.set(sessionId, {
        ...row,
        ...(patch.target !== undefined ? { target: structuredClone(patch.target) } : {}),
        ...(patch.state !== undefined ? { state: structuredClone(patch.state) } : {}),
        version: expectVersion + 1,
      });
      writes.push(patch.target !== undefined ? "repoint" : "render");
      return true;
    },
    async remove(sessionId) {
      writes.push("remove");
      return rows.delete(sessionId);
    },
  };
}
