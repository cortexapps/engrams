/**
 * Per-user harness identity token store (ADR 0051 Drip A).
 *
 * The orchestrator OWNS the user's harness auth token (e.g. the Claude
 * `CLAUDE_CODE_OAUTH_TOKEN`) in its OWN Postgres — the `user_harness_token`
 * table, next to the `user` rows it belongs to. This replaces the
 * coordinator's per-user sealed SecretService vault: at session-create the
 * orchestrator resolves the calling user's token here and passes it to the
 * control plane via `CreateSession.harness_env`, which the coordinator
 * injects AND persists into `session_secrets` for resume.
 *
 * The token plaintext is NEVER logged. At-rest posture is the orchestrator's
 * Postgres — the same store better-auth uses for OAuth tokens.
 *
 * The `HarnessTokenStore` interface is the seam both `/me/claude-token`
 * (routes/me.ts) and TaskService.createTask (rpc/tasks.ts) depend on, and
 * the seam tests fake.
 */

import { eq } from "drizzle-orm";

import { getDb } from "./client.ts";
import { userHarnessToken } from "./schema.ts";

/** The store seam injected into the /me routes and TaskService. */
export interface HarnessTokenStore {
  /** Upsert the user's token (insert-or-replace). */
  put(userId: string, token: string): Promise<void>;
  /** Return the user's token plaintext, or null if none is stored. */
  get(userId: string): Promise<string | null>;
  /** True when a token row exists for the user. */
  has(userId: string): Promise<boolean>;
  /** Idempotent: deleting a missing row succeeds. */
  delete(userId: string): Promise<void>;
}

/**
 * Drizzle-backed `HarnessTokenStore`. `db` is injectable for tests; the real
 * singleton (`getDb()`) is used by default.
 */
export function makeHarnessTokenStore(
  db: ReturnType<typeof getDb> = getDb(),
): HarnessTokenStore {
  return {
    async put(userId, token) {
      await db
        .insert(userHarnessToken)
        .values({ userId, token })
        .onConflictDoUpdate({
          target: userHarnessToken.userId,
          set: { token, updatedAt: new Date() },
        });
    },

    async get(userId) {
      const rows = await db
        .select({ token: userHarnessToken.token })
        .from(userHarnessToken)
        .where(eq(userHarnessToken.userId, userId))
        .limit(1);
      return rows[0]?.token ?? null;
    },

    async has(userId) {
      const rows = await db
        .select({ userId: userHarnessToken.userId })
        .from(userHarnessToken)
        .where(eq(userHarnessToken.userId, userId))
        .limit(1);
      return rows.length > 0;
    },

    async delete(userId) {
      await db
        .delete(userHarnessToken)
        .where(eq(userHarnessToken.userId, userId));
    },
  };
}

/**
 * The reserved harness env-var name carrying the user's Claude OAuth token.
 *
 * This hardcoded name lives HERE in the orchestrator (Drip A): the coordinator
 * is now harness-agnostic and just injects whatever `harness_env` map it is
 * handed. A harness-agnostic declarative spec (the image manifest declaring
 * which identity env-vars its harness wants) is the future; until then the
 * single supported harness is Claude and this const is the one mapping.
 */
export const CLAUDE_OAUTH_ENV_VAR = "CLAUDE_CODE_OAUTH_TOKEN";
