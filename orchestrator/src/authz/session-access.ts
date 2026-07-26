/**
 * Session access rule (ADR 0100 decision 10).
 *
 * Session reads are owner-scoped: `ability.can(action, Session{createdByUserId})`.
 * That is the whole rule for sessions a member started — and it silently denied
 * the one case the review dashboard depends on.
 *
 * A `pr_review` task is inserted with `createdByUserId: null` (nobody owns a
 * review; reviews are an org-visible team dashboard), so `resolveSessionOwner`
 * returns null for its finder/verifier workers and no member's ability matches.
 * The result was a 404 on the exact transcript that justifies a finding, even
 * though the same member could already read the finding, its severity, its
 * verifier reasoning, and the code it quotes.
 *
 * So a session named as a review's `finder_session_id` / `verifier_session_id`
 * is READABLE by anyone who can read that review. This is a derived permission,
 * not sharing: it is computed from a review the caller can already see, and it
 * lives here rather than in `authz/ability.ts` deliberately — growing the
 * ability set with team/sharing semantics is the OpenFGA trigger that file
 * warns about, and this decision does not cross it.
 *
 * Deliberately narrow:
 *   - `read` only. `prompt`, `shell`, and `delete` stay owner-scoped, so a
 *     reviewer session remains un-promptable and un-deletable by members.
 *   - Gated on `read` of `Review`, so if review visibility is ever scoped down,
 *     transcript visibility follows it without another edit here.
 *
 * It does widen `read` on the *whole* session — transcript, metadata, logs,
 * artifacts — for reviewer sessions. That is the same class of data the review
 * page already shows, which is why it is acceptable; it is not narrower than it
 * looks, and shouldn't be described as if it were.
 */

import { subject } from "@casl/ability";

import { eq, or } from "drizzle-orm";
import { getDb } from "../db/client.ts";
import { review } from "../db/schema.ts";
import { log as rootLog } from "../log.ts";
import type { Actions, AppAbility } from "./ability.ts";

const log = rootLog.child({ component: "session-access" });

/** Injectable "is this session a review's finder/verifier worker?" lookup. */
export type IsReviewWorkerSession = (sessionId: string) => Promise<boolean>;

/**
 * The full session-access decision, shared by the HTTP guard
 * (`routes/guard.ts`) and the gRPC passthrough gate (`rpc/passthrough.ts`) so
 * the rule has exactly one home.
 *
 * `ownerId` is the already-resolved owning user (null when unowned). The
 * review-worker lookup runs only on the deny path, so an ordinary owned-session
 * request costs nothing extra.
 */
export async function canAccessSession(
  ability: AppAbility,
  action: Actions,
  sessionId: string,
  ownerId: string | null,
  isReviewWorker: IsReviewWorkerSession,
): Promise<boolean> {
  if (ability.can(action, subject("Session", { createdByUserId: ownerId }))) {
    return true;
  }
  // Derived read (ADR 0100 d10) — reads only, and only for a caller who can
  // already read reviews.
  if (action !== "read") return false;
  if (!ability.can("read", "Review")) return false;
  // A reviewer session is owned by nobody: `insertReviewTask` sets
  // `createdByUserId: null`. So a session that resolved to a real owner cannot
  // be one, and we skip the lookup — otherwise every cross-owner denial would
  // pay a DB query, which is both wasteful and a free amplifier for anyone
  // probing session ids.
  if (ownerId !== null) return false;
  return isReviewWorker(sessionId);
}

// ---------------------------------------------------------------------------
// DB-backed lookup
// ---------------------------------------------------------------------------

const TTL_MS = 5_000;
const MAX_SIZE = 1_000;

interface CacheEntry {
  value: boolean;
  expiresAt: number;
}

// Same shape as the owner cache in `resolve.ts`: a Map-based LRU that evicts
// the oldest entry at the cap. SSE reconnects and event paging hit this on
// every request, and the answer for a given session never changes once the
// review record exists.
const cache = new Map<string, CacheEntry>();

/**
 * True when some review names this session as its finder or verifier worker.
 *
 * Reads `review.finder_session_id` / `verifier_session_id` rather than the
 * `review_session` binding table: that binding row is DELETED when a phase ends
 * (`removeWorkerSession`), and a terminal pass is exactly when someone reads the
 * transcript. The columns on `review` are the durable record of which session
 * ran — that is their documented purpose.
 */
export async function isReviewWorkerSession(sessionId: string): Promise<boolean> {
  const now = Date.now();

  const cached = cache.get(sessionId);
  if (cached !== undefined && cached.expiresAt > now) {
    return cached.value;
  }

  let value: boolean;
  try {
    const db = getDb();
    const rows = await db
      .select({ id: review.id })
      .from(review)
      .where(
        or(
          eq(review.finderSessionId, sessionId),
          eq(review.verifierSessionId, sessionId),
        ),
      )
      .limit(1);
    value = rows.length > 0;
  } catch (err) {
    // Fail closed, and don't cache the failure: this runs inside an authz
    // decision, so a transient DB error must deny (404) rather than surface as
    // a 500 on the SSE/read path. Denying is always safe here — the owner check
    // already ran and said no.
    log.warn({ sessionId, err }, "review-worker session lookup failed; denying");
    return false;
  }

  if (cache.size >= MAX_SIZE) {
    const oldestKey = cache.keys().next().value;
    if (oldestKey !== undefined) cache.delete(oldestKey);
  }
  cache.set(sessionId, { value, expiresAt: now + TTL_MS });
  return value;
}

/** Clear the review-worker cache. Exported for tests. */
export function clearReviewWorkerCache(): void {
  cache.clear();
}
