/**
 * Slack → engrams identity seam (ADR 0060 Decision 4, P2.4).
 *
 * One function, no schema: map a provider user to an engrams user id by email
 * match. Slack: `users.info` → `profile.email` (needs `users:read.email`) →
 * the better-auth `user` table by email. The resolved id becomes the
 * triggered task's `created_by_user_id`, so existing CASL applies unchanged.
 * Unlinked (no email / no matching user) → null; the caller posts "log in
 * first" and does not start a session.
 *
 * Both external reads are injectable so the resolution logic is unit-testable
 * without a live Slack or a DB; production uses the real defaults.
 */

import { sql } from "drizzle-orm";
import type { WebClient } from "@slack/web-api";

import { getDb } from "../db/client.ts";
import { user as userTable } from "../db/schema.ts";
import { getSlackClient } from "./slack.ts";

export interface ResolveIdentityDeps {
  /** Override the Slack client (tests inject a fake; default = getSlackClient()). */
  slack?: WebClient;
  /** Override the email→user lookup (default = the better-auth `user` table). */
  lookupByEmail?: (email: string) => Promise<string | null>;
}

/**
 * The engrams user id for a provider user, or null if unlinked. Only `slack`
 * is supported in v1 (other providers → null, designed-for not built).
 */
export async function resolveEngramsUser(
  provider: string,
  externalUserId: string,
  deps?: ResolveIdentityDeps,
): Promise<string | null> {
  if (provider !== "slack") return null;
  const email = await slackProfileEmail(externalUserId, deps?.slack);
  if (!email) return null;
  const lookup = deps?.lookupByEmail ?? lookupUserIdByEmail;
  return lookup(email);
}

/** Read a Slack user's profile email (needs `users:read.email`), or null. */
async function slackProfileEmail(slackUserId: string, client?: WebClient): Promise<string | null> {
  const c = client ?? (await getSlackClient());
  const resp = await c.users.info({ user: slackUserId });
  const email = resp.user?.profile?.email;
  return typeof email === "string" && email.length > 0 ? email : null;
}

/** Default lookup: a better-auth user by email (case-insensitive), or null. */
async function lookupUserIdByEmail(email: string): Promise<string | null> {
  const rows = await getDb()
    .select({ id: userTable.id })
    .from(userTable)
    .where(sql`lower(${userTable.email}) = lower(${email})`)
    .limit(1);
  return rows[0]?.id ?? null;
}
