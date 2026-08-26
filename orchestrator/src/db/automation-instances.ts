/** Automation instances (ADR 0120 addendum — "workstreams" in the UI): the
 * durable product-level entity many short runs contribute to.
 *
 * Identity is the rendered key template; the partial unique
 * (automation_id, key) WHERE status='open' enforces ONE open instance per
 * key while closed instances accumulate as history. `openInstance` is the
 * race-safe idempotent open (insert-on-conflict-do-nothing + reselect):
 * concurrent kickoffs of the same key JOIN the winner.
 *
 * The handle ledger routes correlation-bearing events: one external
 * identifier (slack:<ch>:<ts>, github:<repo>#<n>) maps to exactly one
 * instance per automation, forever. A second claim refuses loudly
 * (`conflict`) — never a silent rebind (the Zeebe duplicate-subscription
 * bug class) — and a retried step that already owns the handle reports
 * `already_ours` so DBOS replays converge.
 */

import { and, asc, desc, eq, inArray, lt, sql } from "drizzle-orm";

import { getDb } from "./client.ts";
import { automationDrop, automationInstance, automationInstanceHandle } from "./schema.ts";

export interface AutomationInstanceRow {
  id: string;
  automationId: string;
  key: string;
  status: "open" | "closed";
  inputs: Record<string, unknown>;
  openedBy: string;
  openedAt: Date;
  closedAt: Date | null;
  closeReason: string | null;
}

export type RecordHandleResult =
  | { kind: "recorded" }
  | { kind: "already_ours" }
  | { kind: "conflict"; instanceId: string };

export interface AutomationDropRow {
  id: number;
  automationId: string;
  entrypointId: string;
  eventKey: string;
  reason: string;
  detail: string;
  droppedAt: Date;
}

export interface ResolvedHandle {
  handle: string;
  instanceId: string;
  /** The owning instance's lifecycle — admission drops events whose handle
   * resolves to a closed instance (v1 policy, audited). */
  instanceStatus: "open" | "closed";
}

export interface AutomationInstanceStore {
  /** Idempotent open: returns the existing OPEN instance for the key, or
   * inserts one. Never mutates an existing instance's inputs (first-wins —
   * kicking off the same key again JOINS the incumbent). */
  openInstance(input: {
    automationId: string;
    key: string;
    inputs: Record<string, unknown>;
    openedBy: string;
  }): Promise<AutomationInstanceRow>;
  getInstance(id: string): Promise<AutomationInstanceRow | null>;
  getOpenInstanceByKey(automationId: string, key: string): Promise<AutomationInstanceRow | null>;
  /** Oldest-first, capped — the cron fan-out enumeration. */
  listOpenInstances(automationId: string, limit?: number): Promise<AutomationInstanceRow[]>;
  /** The UI listing: open first (oldest-first), then closed (newest close
   * first) when included. */
  listInstances(
    automationId: string,
    opts?: { includeClosed?: boolean; limit?: number },
  ): Promise<AutomationInstanceRow[]>;
  /** The workstream's routing history, oldest first. */
  listInstanceHandles(instanceId: string): Promise<
    Array<{ handle: string; writtenBy: string; createdAt: Date }>
  >;
  /** open → closed CAS; false = it was not open (already closed / unknown). */
  closeInstance(input: {
    instanceId: string;
    reason?: string;
  }): Promise<boolean>;
  recordInstanceHandle(input: {
    automationId: string;
    handle: string;
    instanceId: string;
    writtenBy: string;
  }): Promise<RecordHandleResult>;
  /** One query over all of an event's candidate handles. */
  resolveHandles(automationId: string, handles: string[]): Promise<ResolvedHandle[]>;
  /** The drops ring: record an admission drop (no run row exists for it) and
   * cap the ring per automation. Callers treat this as best-effort — a
   * failed audit write must never fail the delivery. */
  recordDrop(input: {
    automationId: string;
    entrypointId: string;
    eventKey: string;
    reason: string;
    detail: string;
  }): Promise<void>;
  listRecentDrops(automationId: string, limit?: number): Promise<AutomationDropRow[]>;
}

export const INSTANCE_KEY_MAX_CHARS = 512;
export const INSTANCE_HANDLE_MAX_CHARS = 512;
export const INSTANCE_LIST_MAX = 500;
export const DROP_RING_CAP = 50;
const DROP_DETAIL_MAX = 512;

/** ai_<base32> — no ':' or '/', so run ids (`autorun:…:i-<id>:…`) and state
 * prefixes (`i/<id>/`) that embed it stay injective. */
export function newInstanceId(): string {
  const alphabet = "abcdefghijklmnopqrstuvwxyz234567";
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  let out = "";
  for (const b of bytes) out += alphabet[b % 32];
  return `ai_${out}`;
}

export class InstanceLimitError extends Error {
  constructor(
    readonly code: string,
    message: string,
  ) {
    super(message);
  }
}

function checkKey(key: string): void {
  if (key.length === 0) throw new InstanceLimitError("instance_key_empty", "instance key is empty");
  if (key.length > INSTANCE_KEY_MAX_CHARS) {
    throw new InstanceLimitError(
      "instance_key_too_long",
      `instance key is ${key.length} chars (max ${INSTANCE_KEY_MAX_CHARS})`,
    );
  }
}

function checkHandle(handle: string): void {
  if (handle.length === 0) {
    throw new InstanceLimitError("instance_handle_empty", "instance handle is empty");
  }
  if (handle.length > INSTANCE_HANDLE_MAX_CHARS) {
    throw new InstanceLimitError(
      "instance_handle_too_long",
      `instance handle is ${handle.length} chars (max ${INSTANCE_HANDLE_MAX_CHARS})`,
    );
  }
}

function instanceRow(row: typeof automationInstance.$inferSelect): AutomationInstanceRow {
  return {
    id: row.id,
    automationId: row.automationId,
    key: row.key,
    status: row.status === "closed" ? "closed" : "open",
    inputs: row.inputs,
    openedBy: row.openedBy,
    openedAt: row.openedAt,
    closedAt: row.closedAt ?? null,
    closeReason: row.closeReason ?? null,
  };
}

export interface AutomationInstanceStoreDeps {
  db?: ReturnType<typeof getDb>;
  now?: () => Date;
  newId?: () => string;
}

export function makeAutomationInstanceStore(
  deps: AutomationInstanceStoreDeps = {},
): AutomationInstanceStore {
  const db = deps.db ?? getDb();
  const now = deps.now ?? (() => new Date());
  const newId = deps.newId ?? newInstanceId;

  async function reselectOpen(
    automationId: string,
    key: string,
  ): Promise<AutomationInstanceRow | null> {
    const [row] = await db
      .select()
      .from(automationInstance)
      .where(
        and(
          eq(automationInstance.automationId, automationId),
          eq(automationInstance.key, key),
          eq(automationInstance.status, "open"),
        ),
      )
      .limit(1);
    return row ? instanceRow(row) : null;
  }

  return {
    async openInstance(input) {
      checkKey(input.key);
      // Bounded retry: an insert can lose the partial-unique race to a
      // sibling that then closes before our reselect — vanishingly rare, but
      // looping keeps open() total.
      for (let attempt = 0; attempt < 3; attempt++) {
        const inserted = await db
          .insert(automationInstance)
          .values({
            id: newId(),
            automationId: input.automationId,
            key: input.key,
            inputs: input.inputs,
            openedBy: input.openedBy,
            openedAt: now(),
          })
          .onConflictDoNothing()
          .returning();
        if (inserted[0]) return instanceRow(inserted[0]);
        const existing = await reselectOpen(input.automationId, input.key);
        if (existing) return existing;
      }
      throw new Error(
        `open instance for ${input.automationId} key ${JSON.stringify(input.key)} kept racing`,
      );
    },

    async getInstance(id) {
      const [row] = await db
        .select()
        .from(automationInstance)
        .where(eq(automationInstance.id, id))
        .limit(1);
      return row ? instanceRow(row) : null;
    },

    async getOpenInstanceByKey(automationId, key) {
      return reselectOpen(automationId, key);
    },

    async listOpenInstances(automationId, limit = INSTANCE_LIST_MAX) {
      const rows = await db
        .select()
        .from(automationInstance)
        .where(
          and(
            eq(automationInstance.automationId, automationId),
            eq(automationInstance.status, "open"),
          ),
        )
        .orderBy(asc(automationInstance.openedAt), asc(automationInstance.id))
        .limit(limit);
      return rows.map(instanceRow);
    },

    async listInstances(automationId, opts = {}) {
      const limit = opts.limit ?? INSTANCE_LIST_MAX;
      const open = await this.listOpenInstances(automationId, limit);
      if (!opts.includeClosed || open.length >= limit) return open.slice(0, limit);
      const closed = await db
        .select()
        .from(automationInstance)
        .where(
          and(
            eq(automationInstance.automationId, automationId),
            eq(automationInstance.status, "closed"),
          ),
        )
        .orderBy(desc(automationInstance.closedAt), desc(automationInstance.id))
        .limit(limit - open.length);
      return [...open, ...closed.map(instanceRow)];
    },

    async listInstanceHandles(instanceId) {
      const rows = await db
        .select({
          handle: automationInstanceHandle.handle,
          writtenBy: automationInstanceHandle.writtenBy,
          createdAt: automationInstanceHandle.createdAt,
        })
        .from(automationInstanceHandle)
        .where(eq(automationInstanceHandle.instanceId, instanceId))
        .orderBy(asc(automationInstanceHandle.createdAt), asc(automationInstanceHandle.handle));
      return rows;
    },

    async closeInstance(input) {
      const rows = await db
        .update(automationInstance)
        .set({
          status: "closed",
          closedAt: now(),
          ...(input.reason !== undefined ? { closeReason: input.reason } : {}),
        })
        .where(
          and(
            eq(automationInstance.id, input.instanceId),
            eq(automationInstance.status, "open"),
          ),
        )
        .returning({ id: automationInstance.id });
      return rows.length > 0;
    },

    async recordInstanceHandle(input) {
      checkHandle(input.handle);
      const inserted = await db
        .insert(automationInstanceHandle)
        .values({
          automationId: input.automationId,
          handle: input.handle,
          instanceId: input.instanceId,
          writtenBy: input.writtenBy,
          createdAt: now(),
        })
        .onConflictDoNothing()
        .returning();
      if (inserted[0]) return { kind: "recorded" };
      const [holder] = await db
        .select()
        .from(automationInstanceHandle)
        .where(
          and(
            eq(automationInstanceHandle.automationId, input.automationId),
            eq(automationInstanceHandle.handle, input.handle),
          ),
        )
        .limit(1);
      // The ledger is append-only (no deletes), so the holder cannot vanish
      // between our insert and read.
      if (!holder) throw new Error(`handle ${input.handle} disappeared after insert conflict`);
      if (holder.instanceId === input.instanceId) return { kind: "already_ours" };
      return { kind: "conflict", instanceId: holder.instanceId };
    },

    async recordDrop(input) {
      await db.insert(automationDrop).values({
        automationId: input.automationId,
        entrypointId: input.entrypointId,
        eventKey: input.eventKey,
        reason: input.reason,
        detail: input.detail.slice(0, DROP_DETAIL_MAX),
        droppedAt: now(),
      });
      // Cap the ring on the write path (no sweeper): drop everything older
      // than the newest DROP_RING_CAP rows for this automation.
      const cutoff = db
        .select({ id: automationDrop.id })
        .from(automationDrop)
        .where(eq(automationDrop.automationId, input.automationId))
        .orderBy(desc(automationDrop.id))
        .limit(1)
        .offset(DROP_RING_CAP - 1);
      await db
        .delete(automationDrop)
        .where(
          and(
            eq(automationDrop.automationId, input.automationId),
            lt(automationDrop.id, sql`(select min(id) from (${cutoff}) newest)`),
          ),
        );
    },

    async listRecentDrops(automationId, limit = DROP_RING_CAP) {
      const rows = await db
        .select()
        .from(automationDrop)
        .where(eq(automationDrop.automationId, automationId))
        .orderBy(desc(automationDrop.id))
        .limit(limit);
      return rows.map((row) => ({
        id: row.id,
        automationId: row.automationId,
        entrypointId: row.entrypointId,
        eventKey: row.eventKey,
        reason: row.reason,
        detail: row.detail,
        droppedAt: row.droppedAt,
      }));
    },

    async resolveHandles(automationId, handles) {
      if (handles.length === 0) return [];
      const rows = await db
        .select({
          handle: automationInstanceHandle.handle,
          instanceId: automationInstanceHandle.instanceId,
          instanceStatus: automationInstance.status,
        })
        .from(automationInstanceHandle)
        .innerJoin(
          automationInstance,
          eq(automationInstanceHandle.instanceId, automationInstance.id),
        )
        .where(
          and(
            eq(automationInstanceHandle.automationId, automationId),
            inArray(automationInstanceHandle.handle, handles),
          ),
        );
      return rows.map((r) => ({
        handle: r.handle,
        instanceId: r.instanceId,
        instanceStatus: r.instanceStatus === "closed" ? ("closed" as const) : ("open" as const),
      }));
    },
  };
}
