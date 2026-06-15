/**
 * Per-user, env-var-name-keyed session secret store, KEK-envelope sealed at
 * rest (ADR 0051 Drip A).
 *
 * The orchestrator OWNS the user's harness identity secrets (today just the
 * Claude `CLAUDE_CODE_OAUTH_TOKEN`) in its OWN Postgres — the
 * `user_session_secrets` table, next to the `user` rows they belong to. This
 * replaces the coordinator's per-user sealed SecretService vault: at
 * session-create the orchestrator opens all of a user's secrets here and passes
 * them to the control plane via `CreateSession.harness_env`, which the
 * coordinator injects AND persists into `session_secrets` for resume.
 *
 * SECURITY: every stored value is KEK-envelope sealed with the SAME key + SAME
 * envelope format as the Rust coordinator (`engram-crypto`,
 * `ENGRAM_KEK_MASTER_KEY`) — see src/crypto/seal.ts. The DB row holds ciphertext
 * only; the master key lives outside the DB (env var, sourced from GCP Secret
 * Manager in prod). Plaintext is NEVER logged.
 *
 * The `UserSecretStore` interface is the seam both `/me/claude-token`
 * (routes/me.ts) and TaskService.createTask (rpc/tasks.ts) depend on, and the
 * seam tests fake. The `Sealer` is also injectable (tests pass a fixed key).
 */

import { and, eq } from "drizzle-orm";

import { getDb } from "./client.ts";
import { userSessionSecrets } from "./schema.ts";
import { defaultSealer, type Sealer } from "../crypto/seal.ts";

/** The store seam injected into the /me routes and TaskService. */
export interface UserSecretStore {
  /** Seal + upsert (insert-or-replace) the value for (userId, envVarName). */
  put(userId: string, envVarName: string, plaintext: string): Promise<void>;
  /**
   * Open every secret for the user and return an `envVarName → plaintext` map.
   * This is what createTask passes as `harness_env`. Empty when none stored.
   */
  getAll(userId: string): Promise<Record<string, string>>;
  /** Open a single secret, or null if none is stored for that name. */
  get(userId: string, envVarName: string): Promise<string | null>;
  /** True when a row exists for (userId, envVarName). */
  has(userId: string, envVarName: string): Promise<boolean>;
  /** Idempotent: deleting a missing row succeeds. */
  delete(userId: string, envVarName: string): Promise<void>;
}

/**
 * Drizzle-backed `UserSecretStore`. `db` and `sealer` are injectable for tests;
 * the real singletons (`getDb()` / `defaultSealer()`) are used by default. The
 * sealer is resolved lazily so importing this module does not require
 * ENGRAM_KEK_MASTER_KEY at import time.
 */
export function makeUserSecretStore(
  db: ReturnType<typeof getDb> = getDb(),
  sealer?: Sealer,
): UserSecretStore {
  const resolveSealer = (): Sealer => sealer ?? defaultSealer();

  return {
    async put(userId, envVarName, plaintext) {
      const sealed = resolveSealer().seal(plaintext);
      const row = {
        userId,
        envVarName,
        wrappedDek: sealed.wrappedDek.toString("base64"),
        nonce: sealed.nonce.toString("base64"),
        ciphertext: sealed.ciphertext.toString("base64"),
        keyId: sealed.keyId,
      };
      await db
        .insert(userSessionSecrets)
        .values(row)
        .onConflictDoUpdate({
          target: [userSessionSecrets.userId, userSessionSecrets.envVarName],
          set: {
            wrappedDek: row.wrappedDek,
            nonce: row.nonce,
            ciphertext: row.ciphertext,
            keyId: row.keyId,
            updatedAt: new Date(),
          },
        });
    },

    async getAll(userId) {
      const rows = await db
        .select({
          envVarName: userSessionSecrets.envVarName,
          wrappedDek: userSessionSecrets.wrappedDek,
          nonce: userSessionSecrets.nonce,
          ciphertext: userSessionSecrets.ciphertext,
          keyId: userSessionSecrets.keyId,
        })
        .from(userSessionSecrets)
        .where(eq(userSessionSecrets.userId, userId));

      const sealerInstance = resolveSealer();
      const out: Record<string, string> = {};
      for (const r of rows) {
        out[r.envVarName] = sealerInstance
          .open({
            wrappedDek: Buffer.from(r.wrappedDek, "base64"),
            nonce: Buffer.from(r.nonce, "base64"),
            ciphertext: Buffer.from(r.ciphertext, "base64"),
            keyId: r.keyId,
          })
          .toString("utf8");
      }
      return out;
    },

    async get(userId, envVarName) {
      const rows = await db
        .select({
          wrappedDek: userSessionSecrets.wrappedDek,
          nonce: userSessionSecrets.nonce,
          ciphertext: userSessionSecrets.ciphertext,
          keyId: userSessionSecrets.keyId,
        })
        .from(userSessionSecrets)
        .where(
          and(
            eq(userSessionSecrets.userId, userId),
            eq(userSessionSecrets.envVarName, envVarName),
          ),
        )
        .limit(1);
      const r = rows[0];
      if (!r) return null;
      return resolveSealer()
        .open({
          wrappedDek: Buffer.from(r.wrappedDek, "base64"),
          nonce: Buffer.from(r.nonce, "base64"),
          ciphertext: Buffer.from(r.ciphertext, "base64"),
          keyId: r.keyId,
        })
        .toString("utf8");
    },

    async has(userId, envVarName) {
      const rows = await db
        .select({ userId: userSessionSecrets.userId })
        .from(userSessionSecrets)
        .where(
          and(
            eq(userSessionSecrets.userId, userId),
            eq(userSessionSecrets.envVarName, envVarName),
          ),
        )
        .limit(1);
      return rows.length > 0;
    },

    async delete(userId, envVarName) {
      await db
        .delete(userSessionSecrets)
        .where(
          and(
            eq(userSessionSecrets.userId, userId),
            eq(userSessionSecrets.envVarName, envVarName),
          ),
        );
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
