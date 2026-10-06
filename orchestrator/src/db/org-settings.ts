/** Org settings (ADR 0104 amendment, 2026-10-05): policies an admin chooses
 * on the Settings page, one JSON document per key. The retention policy is
 * the first. Each key's shape is validated here, so a stored document from
 * an older release still reads (unknown fields drop, missing ones default). */

import { eq } from "drizzle-orm";
import * as z from "zod";

import { getDb } from "./client.ts";
import { orgSetting as orgSettingTable } from "./schema.ts";

export const RETENTION_KEY = "retention";

/** Run detail retention: the step ledger of a finished automation run (its
 * block inputs/outputs, the relay's per-event records) and the engine's
 * own records of that run (DBOS workflow status + step outputs) are deleted
 * once the run has been over for this many days. The run itself — status,
 * timing, trigger, the session it bound — stays. A week is the floor: the
 * sweep's terminal-failure scan looks back seven days. */
export const RETENTION_RUN_DETAIL_DAYS_MIN = 7;
export const RETENTION_RUN_DETAIL_DAYS_MAX = 365;
export const RETENTION_RUN_DETAIL_DAYS_DEFAULT = 30;

export const retentionPolicySchema = z.object({
  runDetailDays: z
    .number()
    .int()
    .min(RETENTION_RUN_DETAIL_DAYS_MIN)
    .max(RETENTION_RUN_DETAIL_DAYS_MAX)
    .default(RETENTION_RUN_DETAIL_DAYS_DEFAULT),
});
export type RetentionPolicy = z.infer<typeof retentionPolicySchema>;

export const DEFAULT_RETENTION_POLICY: RetentionPolicy = retentionPolicySchema.parse({});

/** A stored document → the policy, tolerating an older or malformed shape
 * (the defaults fill in; a value outside the bounds reads as the default). */
export function parseRetentionPolicy(value: unknown): RetentionPolicy {
  const parsed = retentionPolicySchema.safeParse(
    typeof value === "object" && value !== null && !Array.isArray(value) ? value : {},
  );
  return parsed.success ? parsed.data : DEFAULT_RETENTION_POLICY;
}

export interface OrgSettingStore {
  get(key: string): Promise<unknown | null>;
  set(key: string, value: unknown, updatedByUserId: string | null): Promise<void>;
}

export function makeOrgSettingStore(db: ReturnType<typeof getDb> = getDb()): OrgSettingStore {
  return {
    async get(key) {
      const rows = await db
        .select({ value: orgSettingTable.value })
        .from(orgSettingTable)
        .where(eq(orgSettingTable.key, key))
        .limit(1);
      return rows[0] === undefined ? null : rows[0].value;
    },
    async set(key, value, updatedByUserId) {
      await db
        .insert(orgSettingTable)
        .values({ key, value, updatedByUserId, updatedAt: new Date() })
        .onConflictDoUpdate({
          target: orgSettingTable.key,
          set: { value, updatedByUserId, updatedAt: new Date() },
        });
    },
  };
}

/** The retention policy as the collector and the RPC read it. */
export async function readRetentionPolicy(store: Pick<OrgSettingStore, "get">): Promise<RetentionPolicy> {
  return parseRetentionPolicy(await store.get(RETENTION_KEY));
}

/** Deterministic in-memory store for tests. */
export function makeInMemoryOrgSettingStore(initial: Record<string, unknown> = {}): OrgSettingStore & {
  rows: Map<string, unknown>;
} {
  const rows = new Map(Object.entries(initial));
  return {
    rows,
    async get(key) {
      return rows.has(key) ? rows.get(key)! : null;
    },
    async set(key, value) {
      rows.set(key, value);
    },
  };
}
