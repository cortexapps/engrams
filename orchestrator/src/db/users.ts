/**
 * Read-only user identity lookups against the better-auth `user` table.
 *
 * ADR 0031 §7 uses the single-id lookup for create-path git attribution.
 * TaskService reads use the batch lookup to embed best-effort owner identity
 * snapshots. The store stays injectable so both paths are unit-testable.
 */

import { eq, inArray } from "drizzle-orm";
import type { NodePgDatabase } from "drizzle-orm/node-postgres";

import * as schema from "./schema.ts";
import { user } from "./schema.ts";

export interface UserIdentity {
  name: string;
  email: string;
}

/** The identity seam TaskService depends on (and tests fake). */
export interface UserIdentityStore {
  /** The user's display name + email, or null for an unknown id. */
  getIdentity(userId: string): Promise<UserIdentity | null>;
  /** Known display identities keyed by user id. */
  getIdentities(userIds: string[]): Promise<Map<string, UserIdentity>>;
}

export function makeUserIdentityStore(db: NodePgDatabase<typeof schema>): UserIdentityStore {
  return {
    async getIdentity(userId) {
      const rows = await db
        .select({ name: user.name, email: user.email })
        .from(user)
        .where(eq(user.id, userId))
        .limit(1);
      return rows[0] ?? null;
    },
    async getIdentities(userIds) {
      if (userIds.length === 0) return new Map();
      const rows = await db
        .select({ id: user.id, name: user.name, email: user.email })
        .from(user)
        .where(inArray(user.id, userIds));
      return new Map(rows.map(({ id, name, email }) => [id, { name, email }]));
    },
  };
}
