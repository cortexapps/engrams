/** Restricted profile launch grants (ADR 0109). */

import { and, eq, inArray } from "drizzle-orm";

import { getDb } from "./client.ts";
import { profileLaunchGrant as grantTable } from "./schema.ts";

export interface ProfileLaunchGrantStore {
  listForProfiles(profileIds: string[]): Promise<Map<string, string[]>>;
  replace(profileId: string, principalIds: string[]): Promise<void>;
  canLaunch(profileId: string, principalId: string): Promise<boolean>;
}

export function makeProfileLaunchGrantStore(
  db: ReturnType<typeof getDb> = getDb(),
): ProfileLaunchGrantStore {
  return {
    async listForProfiles(profileIds) {
      if (profileIds.length === 0) return new Map();
      const rows = await db
        .select()
        .from(grantTable)
        .where(inArray(grantTable.profileId, profileIds));
      const out = new Map<string, string[]>();
      for (const row of rows) {
        const ids = out.get(row.profileId) ?? [];
        ids.push(row.principalId);
        out.set(row.profileId, ids);
      }
      for (const ids of out.values()) ids.sort();
      return out;
    },

    async replace(profileId, principalIds) {
      const unique = [...new Set(principalIds)].sort();
      await db.transaction(async (tx) => {
        await tx.delete(grantTable).where(eq(grantTable.profileId, profileId));
        if (unique.length > 0) {
          await tx.insert(grantTable).values(
            unique.map((principalId) => ({ profileId, principalId })),
          );
        }
      });
    },

    async canLaunch(profileId, principalId) {
      const rows = await db
        .select({ profileId: grantTable.profileId })
        .from(grantTable)
        .where(
          and(
            eq(grantTable.profileId, profileId),
            eq(grantTable.principalId, principalId),
          ),
        )
        .limit(1);
      return rows.length > 0;
    },
  };
}
