/**
 * Session-owner resolver (ADR 0051 Task 18).
 *
 * Given a control-plane sessionId, looks up the owning task's
 * `createdByUserId` via the orchestrator's DB (task_session JOIN task).
 * The result is cached with a 5-second TTL (LRU-style, size-capped at 1 000)
 * so high-frequency streaming callers don't hammer Postgres on every auth
 * check.
 *
 * Cache invalidation:
 *   - Entries expire after TTL_MS (5 s).
 *   - clearOwnerCache() is exported for tests so they can reset state between
 *     cases without restarting the process.
 *
 * Returns `null` when no task_session row maps this sessionId — meaning the
 * session is either unowned by any orchestrator task or has never been seen.
 * The passthrough gate treats null as "not found / not owned" and returns
 * NotFound (anti-enumeration behaviour per ADR §6).
 */

import { and, eq, isNull, or } from "drizzle-orm";
import { config } from "../config.ts";
import { getDb } from "../db/client.ts";
import { spec, task, taskSession, user } from "../db/schema.ts";

const TTL_MS = 5_000;
const MAX_SIZE = 1_000;

interface CacheEntry {
  value: string | null; // createdByUserId, or null if no owner row
  expiresAt: number;
}

// Simple Map-based LRU: evict oldest entry when cap is hit.
const cache = new Map<string, CacheEntry>();

/**
 * Look up which user owns the session (via task_session → task join).
 * Returns the owning task's `createdByUserId`, or null if not found.
 */
export async function resolveSessionOwner(sessionId: string): Promise<string | null> {
  const now = Date.now();

  // Check cache first.
  const cached = cache.get(sessionId);
  if (cached !== undefined && cached.expiresAt > now) {
    return cached.value;
  }

  // Query: task_session JOIN task on taskId = task.id WHERE sessionId = ?
  const db = getDb();
  const rows = await db
    .select({ createdByUserId: task.createdByUserId })
    .from(taskSession)
    .innerJoin(task, eq(taskSession.taskId, task.id))
    .where(eq(taskSession.sessionId, sessionId))
    .limit(1);

  const value = rows[0]?.createdByUserId ?? null;

  // Enforce size cap: remove the oldest entry if we're at the limit.
  if (cache.size >= MAX_SIZE) {
    const oldestKey = cache.keys().next().value;
    if (oldestKey !== undefined) {
      cache.delete(oldestKey);
    }
  }

  cache.set(sessionId, { value, expiresAt: now + TTL_MS });
  return value;
}

/** Return true when the user belongs to the deployment that owns the spec. */
export async function resolveSpecMembership(specId: string, userId: string): Promise<boolean> {
  const rows = await getDb()
    .select({ id: spec.id })
    .from(spec)
    .innerJoin(user, eq(user.id, userId))
    .where(
      and(
        eq(spec.id, specId),
        eq(spec.orgId, config.deploymentId),
        or(eq(user.banned, false), isNull(user.banned)),
      ),
    )
    .limit(1);
  return rows.length === 1;
}

/** Return true when the deployment owns the spec and it is in drafting. */
export async function resolveDraftingSpec(specId: string): Promise<boolean> {
  const rows = await getDb()
    .select({ id: spec.id })
    .from(spec)
    .where(
      and(eq(spec.id, specId), eq(spec.orgId, config.deploymentId), eq(spec.phase, "drafting")),
    )
    .limit(1);
  return rows.length === 1;
}

/**
 * Clear the owner cache. Exported for tests.
 */
export function clearOwnerCache(): void {
  cache.clear();
}

/**
 * Evict a single session from the owner cache.
 *
 * Task 19's createTask MUST call this after inserting task_session rows so
 * that a just-created session is not served a stale null from the ≤5 s
 * negative-cache window.
 */
export function evictOwnerCacheEntry(sessionId: string): void {
  cache.delete(sessionId);
}
