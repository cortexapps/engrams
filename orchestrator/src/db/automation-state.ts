/** Automation state (ADR 0119 D10): a per-automation KV over
 * `automation_state`, one JSON document per entity.
 *
 * Concurrency is layered. The default path needs no machinery here: runs
 * that mutate an entity hold the entity's concurrency claim, so they are
 * the sole writer. This store adds the second layer — versioned CAS for the
 * writers that cannot hold the entity claim (a cron sweep spanning many
 * entities, a session tool with no live run). Every write stamps
 * `writer = <runId>:<framePath>`; a CAS that finds `version == expect + 1`
 * with its own writer tag reports success, so a DBOS step re-executed after
 * a crash-before-checkpoint replays clean instead of reporting a false
 * conflict (the prompt-outbox idempotency identity, applied to state).
 *
 * There is deliberately no lock primitive and no multi-key transaction: an
 * entity's facts live in one document, and cross-document consistency is
 * the claim layer's job.
 */

import { and, asc, eq, like, sql } from "drizzle-orm";

import { getDb } from "./client.ts";
import { automationState } from "./schema.ts";
import { StateLimitError } from "../automations/engine/deps.ts";
import type { EngineStateEntry, EngineStateSetResult, EngineStateStore } from "../automations/engine/deps.ts";

export const STATE_KEY_MAX_CHARS = 512;
export const STATE_VALUE_MAX_BYTES = 64 * 1024;
export const STATE_MAX_KEYS_PER_AUTOMATION = 5_000;
export const STATE_LIST_MAX_ENTRIES = 500;

export interface AutomationStateStoreDeps {
  db?: ReturnType<typeof getDb>;
  now?: () => Date;
  /** Test seam: shrink the per-automation key cap. */
  maxKeys?: number;
}

function checkKey(key: string): void {
  if (key.length === 0) throw new StateLimitError("state_key_empty", "state key is empty");
  if (key.length > STATE_KEY_MAX_CHARS) {
    throw new StateLimitError(
      "state_key_too_long",
      `state key is ${key.length} chars (max ${STATE_KEY_MAX_CHARS})`,
    );
  }
}

function checkValue(value: unknown): void {
  if (value === undefined) {
    throw new StateLimitError("state_value_undefined", "state value must be JSON (undefined given)");
  }
  const bytes = Buffer.byteLength(JSON.stringify(value) ?? "", "utf8");
  if (bytes > STATE_VALUE_MAX_BYTES) {
    throw new StateLimitError(
      "state_value_too_large",
      `state value is ${bytes} bytes serialized (max ${STATE_VALUE_MAX_BYTES})`,
    );
  }
}

/** LIKE-escape a literal prefix (`%`/`_`/`\` are wildcards otherwise). */
function likePrefix(prefix: string): string {
  return `${prefix.replaceAll("\\", "\\\\").replaceAll("%", "\\%").replaceAll("_", "\\_")}%`;
}

export function makeAutomationStateStore(deps: AutomationStateStoreDeps = {}): EngineStateStore {
  const db = deps.db ?? getDb();
  const now = deps.now ?? (() => new Date());
  const maxKeys = deps.maxKeys ?? STATE_MAX_KEYS_PER_AUTOMATION;

  async function readCurrent(automationId: string, key: string): Promise<EngineStateEntry | null> {
    const [row] = await db
      .select()
      .from(automationState)
      .where(and(eq(automationState.automationId, automationId), eq(automationState.key, key)))
      .limit(1);
    return row ? { key: row.key, value: row.value, version: row.version, writer: row.writer } : null;
  }

  return {
    get: readCurrent,

    async set(automationId, key, value, opts): Promise<EngineStateSetResult> {
      checkKey(key);
      checkValue(value);
      const expect = opts.expectVersion;

      if (expect === undefined) {
        // Last-write-wins upsert. The key cap applies to NEW keys only, so
        // probe first; the probe+insert race can overshoot the cap by the
        // number of concurrent writers, which is fine for a soft cap.
        const existing = await readCurrent(automationId, key);
        if (existing === null) {
          const [{ count }] = (await db
            .select({ count: sql<number>`count(*)::int` })
            .from(automationState)
            .where(eq(automationState.automationId, automationId))) as [{ count: number }];
          if (count >= maxKeys) {
            throw new StateLimitError(
              "state_capacity",
              `automation has ${count} state keys (max ${maxKeys})`,
            );
          }
        }
        const [row] = await db
          .insert(automationState)
          .values({ automationId, key, value, version: 1, writer: opts.writer, updatedAt: now() })
          .onConflictDoUpdate({
            target: [automationState.automationId, automationState.key],
            set: {
              value,
              version: sql`${automationState.version} + 1`,
              writer: opts.writer,
              updatedAt: now(),
            },
          })
          .returning({ version: automationState.version });
        return { ok: true, version: row!.version };
      }

      if (expect === 0) {
        // Create-only.
        const inserted = await db
          .insert(automationState)
          .values({ automationId, key, value, version: 1, writer: opts.writer, updatedAt: now() })
          .onConflictDoNothing()
          .returning({ version: automationState.version });
        if (inserted.length > 0) return { ok: true, version: 1 };
        const current = await readCurrent(automationId, key);
        if (current !== null && current.version === 1 && current.writer === opts.writer) {
          return { ok: true, version: 1 }; // our own create, replayed
        }
        return { ok: false, current };
      }

      const updated = await db
        .update(automationState)
        .set({ value, version: sql`${automationState.version} + 1`, writer: opts.writer, updatedAt: now() })
        .where(
          and(
            eq(automationState.automationId, automationId),
            eq(automationState.key, key),
            eq(automationState.version, expect),
          ),
        )
        .returning({ version: automationState.version });
      if (updated.length > 0) return { ok: true, version: updated[0]!.version };
      const current = await readCurrent(automationId, key);
      if (current !== null && current.version === expect + 1 && current.writer === opts.writer) {
        return { ok: true, version: current.version }; // our own write, replayed
      }
      return { ok: false, current };
    },

    async delete(automationId, key, opts) {
      checkKey(key);
      const conditions = [
        eq(automationState.automationId, automationId),
        eq(automationState.key, key),
        ...(opts.expectVersion !== undefined ? [eq(automationState.version, opts.expectVersion)] : []),
      ];
      const deleted = await db
        .delete(automationState)
        .where(and(...conditions))
        .returning({ key: automationState.key });
      if (deleted.length > 0) return { ok: true, deleted: true };
      const current = await readCurrent(automationId, key);
      // A missing row is an idempotent success either way: unconditional
      // delete of nothing, or our own conditional delete replayed.
      if (current === null) return { ok: true, deleted: false };
      return { ok: false, current };
    },

    async list(automationId, opts = {}) {
      const limit = Math.min(Math.max(opts.limit ?? STATE_LIST_MAX_ENTRIES, 1), STATE_LIST_MAX_ENTRIES);
      const conditions = [
        eq(automationState.automationId, automationId),
        ...(opts.prefix !== undefined && opts.prefix !== ""
          ? [like(automationState.key, likePrefix(opts.prefix))]
          : []),
      ];
      const rows = await db
        .select()
        .from(automationState)
        .where(and(...conditions))
        .orderBy(asc(automationState.key))
        .limit(limit + 1);
      const entries = rows
        .slice(0, limit)
        .map((row) => ({ key: row.key, value: row.value, version: row.version, writer: row.writer }));
      return { entries, truncated: rows.length > limit };
    },
  };
}
