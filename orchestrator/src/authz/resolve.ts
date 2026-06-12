/**
 * Session-owner resolver (ADR 0039 Task 18).
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

import { eq } from "drizzle-orm";
import { getDb } from "../db/client.ts";
import { task, taskSession } from "../db/schema.ts";

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
export async function resolveSessionOwner(
  sessionId: string,
): Promise<string | null> {
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

/**
 * Clear the owner cache. Exported for tests.
 */
export function clearOwnerCache(): void {
  cache.clear();
}
