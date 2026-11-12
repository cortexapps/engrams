/** A chat user as an engrams user: the provider gives the email address and
 * the `user` table gives the id. The session a thread starts is owned by
 * that user. */

import { sql } from "drizzle-orm";

import { getDb } from "../db/client.ts";
import { user as userTable } from "../db/schema.ts";
import type { ChatProvider } from "./chat.ts";
import { chatFor } from "./registry.ts";

/** What a thread gets when its author has no engrams user. */
export const NO_USER_MSG = "You don't have a user in engrams — log in first, then try again.";

export interface ResolveIdentityDeps {
  chat?: (provider: string) => Pick<ChatProvider, "userEmail"> | null;
  lookupByEmail?: (email: string) => Promise<string | null>;
}

/** The engrams user id for a chat user; null when the provider is not a chat
 * provider, gives no email address, or no user has that address. */
export async function resolveEngramsUser(
  provider: string,
  externalUserId: string,
  deps: ResolveIdentityDeps = {},
): Promise<string | null> {
  const email = await (deps.chat ?? chatFor)(provider)?.userEmail(externalUserId);
  if (!email) return null;
  return (deps.lookupByEmail ?? lookupUserIdByEmail)(email);
}

async function lookupUserIdByEmail(email: string): Promise<string | null> {
  const rows = await getDb()
    .select({ id: userTable.id })
    .from(userTable)
    .where(sql`lower(${userTable.email}) = lower(${email})`)
    .limit(1);
  return rows[0]?.id ?? null;
}
