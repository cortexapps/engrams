/**
 * Seed the headless dev credential (Tilt `dev-api-key` resource).
 *
 * Mints an ADMIN service-account API key named `dev-local` — exactly what the
 * admin-gated ApiKeyService.CreateApiKey RPC does, via the same primitives:
 * an un-log-in-able `apikey+<uuid>@service.local` user + a headerless
 * server-side `auth.api.createApiKey` (passing headers would make the plugin
 * treat it as a client call and reject body.userId — the ADR 0086 gotcha).
 *
 * The plaintext lands in the file given as argv[2] (default ../var/dev-api-key,
 * 0600, gitignored via var/). Idempotent: when the key row AND the file both
 * exist it's a no-op; a missing file (plaintext is unrecoverable) or a missing
 * row re-mints, deleting any stale counterpart first.
 *
 * Consumers: justfile recipes + deploy/dev scripts export
 *   ENGRAMS_URL=http://localhost:8787 ENGRAMS_API_KEY=$(cat var/dev-api-key)
 * for the `engrams` CLI. Dev-only — prod keys are minted in /settings/api-keys.
 */

import { chmodSync, existsSync, mkdirSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { eq } from "drizzle-orm";

import { auth } from "../src/auth/better-auth.ts";
import { getDb } from "../src/db/client.ts";
import { apikey, user } from "../src/db/schema.ts";

const KEY_NAME = "dev-local";
const outPath = resolve(process.argv[2] ?? "../var/dev-api-key");

const db = getDb();

const existing = await db
  .select({ id: apikey.id, referenceId: apikey.referenceId })
  .from(apikey)
  .innerJoin(user, eq(apikey.referenceId, user.id))
  .where(eq(apikey.name, KEY_NAME));

if (existing.length > 0 && existsSync(outPath)) {
  console.log(`dev-api-key: ${KEY_NAME} exists and ${outPath} is present — nothing to do`);
  process.exit(0);
}

// Re-mint: drop any stale row first (the plaintext of an existing key is
// unrecoverable, so a missing file means the key is useless). Deleting the
// service user cascades the key row.
for (const row of existing) {
  await db.delete(user).where(eq(user.id, row.referenceId));
}

const ctx = await auth.$context;
const serviceUser = await ctx.internalAdapter.createUser({
  email: `apikey+${crypto.randomUUID()}@service.local`,
  name: KEY_NAME,
  emailVerified: true,
  role: "admin",
});
const minted = await auth.api.createApiKey({
  body: { name: KEY_NAME, userId: serviceUser.id },
});

mkdirSync(dirname(outPath), { recursive: true });
writeFileSync(outPath, `${minted.key}\n`, { mode: 0o600 });
chmodSync(outPath, 0o600); // mode only applies on create; clamp rewrites too
console.log(`dev-api-key: minted ${KEY_NAME} (id=${minted.id}) → ${outPath}`);
process.exit(0);
