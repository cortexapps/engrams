/**
 * Read-only user identity lookups against the better-auth `user` table.
 *
 * ADR 0031 §7 (git commit attribution): the create-task path stamps the
 * owner's name/email into the session env (`ENGRAM_USER_NAME` /
 * `ENGRAM_USER_EMAIL`) so in-guest commits are authored by the human who
 * started the session. This store is that lookup's seam — injectable so
 * task-create stays unit-testable with fakes (rpc/task-create.ts).
 */

import { eq } from "drizzle-orm";
import type { NodePgDatabase } from "drizzle-orm/node-postgres";

import * as schema from "./schema.ts";
import { user } from "./schema.ts";

export interface UserIdentity {
  name: string;
  email: string;
}

/** The seam TaskService's create path depends on (and tests fake). */
export interface UserIdentityStore {
  /** The user's display name + email, or null for an unknown id. */
  getIdentity(userId: string): Promise<UserIdentity | null>;
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
  };
}
