/**
 * A short-lived, per-process cache of sessions resolved from API keys.
 *
 * Without it, every keyed request reads the key row and the owning user from
 * Postgres. An entry lives for at most `ttlMs` and never past the key's own
 * expiry. Entries are keyed by the SHA-256 of the key, and the stored session
 * holds no key material: a hit puts the presented key back as the token.
 *
 * A revoke on this replica calls `forgetKey`, so the key stops working here
 * at once. Another replica, or a change made directly in the database, can
 * keep serving a cached session for up to `ttlMs`.
 */

import { createHash } from "node:crypto";

/** The session fields the cache reads; better-auth's session satisfies it. */
export interface KeyedSession {
  session: { id: string; token: string; expiresAt: Date };
}

interface Entry<S extends KeyedSession> {
  session: S;
  expiresAtMs: number;
}

export const API_KEY_SESSION_TTL_MS = 5_000;
const MAX_ENTRIES = 1_024;

function digest(key: string): string {
  return createHash("sha256").update(key).digest("hex");
}

function withToken<S extends KeyedSession>(s: S, token: string): S {
  return { ...s, session: { ...s.session, token } };
}

export class ApiKeySessionCache<S extends KeyedSession> {
  readonly #entries = new Map<string, Entry<S>>();

  constructor(
    private readonly ttlMs: number = API_KEY_SESSION_TTL_MS,
    private readonly now: () => number = Date.now,
  ) {}

  get(key: string): S | null {
    const id = digest(key);
    const entry = this.#entries.get(id);
    if (!entry) return null;
    if (entry.expiresAtMs <= this.now()) {
      this.#entries.delete(id);
      return null;
    }
    return withToken(entry.session, key);
  }

  set(key: string, session: S): void {
    const now = this.now();
    const expiresAtMs = Math.min(now + this.ttlMs, session.session.expiresAt.getTime());
    if (expiresAtMs <= now) return;
    if (this.#entries.size >= MAX_ENTRIES) this.#evict(now);
    this.#entries.set(digest(key), { session: withToken(session, ""), expiresAtMs });
  }

  /** Drop every entry for the API key with this id (the session id). */
  forgetKey(keyId: string): void {
    for (const [id, entry] of this.#entries) {
      if (entry.session.session.id === keyId) this.#entries.delete(id);
    }
  }

  clear(): void {
    this.#entries.clear();
  }

  #evict(now: number): void {
    for (const [id, entry] of this.#entries) {
      if (entry.expiresAtMs <= now) this.#entries.delete(id);
    }
    if (this.#entries.size < MAX_ENTRIES) return;
    // Map iteration is insertion order: drop the oldest when all are live.
    const oldest = this.#entries.keys().next();
    if (!oldest.done) this.#entries.delete(oldest.value);
  }
}
