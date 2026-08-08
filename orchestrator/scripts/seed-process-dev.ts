/**
 * Seed the browser login and profiles for the local ProcessBackend stack.
 *
 * Tilt runs this only when backend detection selects `process`. The fixed
 * password is therefore a local development credential, not a deployment
 * default. Sign-up goes through Better Auth so its credential row uses the
 * production password hashing path. Profile rows are safe to upsert directly.
 */

import { eq } from "drizzle-orm";

import { getDb } from "../src/db/client.ts";
import { profile, user } from "../src/db/schema.ts";

const EMAIL = "dev@engrams.local";
const PASSWORD = "engrams-dev";
const IMAGE_ID = "00000000-0000-4000-8000-000000000001";
const ORCHESTRATOR_URL =
  process.env["ORCHESTRATOR_URL"] ?? "http://127.0.0.1:8787";
const PUBLIC_ORIGIN =
  process.env["ORCHESTRATOR_PUBLIC_URL"] ?? "http://localhost:5173";

const db = getDb();
let existingUser = await db
  .select({ id: user.id })
  .from(user)
  .where(eq(user.email, EMAIL));

if (existingUser.length === 0) {
  const response = await fetch(`${ORCHESTRATOR_URL}/api/auth/sign-up/email`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      origin: PUBLIC_ORIGIN,
    },
    body: JSON.stringify({ email: EMAIL, password: PASSWORD, name: "Engrams Dev" }),
  });
  if (!response.ok) {
    throw new Error(
      `Process dev user sign-up failed (${response.status}): ${await response.text()}`,
    );
  }
  existingUser = await db
    .select({ id: user.id })
    .from(user)
    .where(eq(user.email, EMAIL));
}

if (existingUser.length !== 1) {
  throw new Error(
    `Expected one Process dev user for ${EMAIL}, found ${existingUser.length}`,
  );
}
await db
  .update(user)
  .set({ role: "admin", emailVerified: true })
  .where(eq(user.id, existingUser[0]!.id));

const commonProfile = {
  imageId: IMAGE_ID,
  model: null,
  effort: null,
  includeUserTokens: true,
  envVars: {},
  skills: ["skills"],
  integrationGrants: [],
  network: { default: "allow" as const, allowHosts: [], allowHostPatterns: [] },
  secrets: [],
  repos: [],
  portExposures: [],
  designation: null,
  deletedAt: null,
  updatedAt: new Date(),
};

const profiles = [
  {
    id: "00000000-0000-4000-8000-000000000003",
    name: "dev-claude",
    description: "Local ProcessBackend smoke profile for Claude Code",
    icon: "Bot",
    harness: "claude",
  },
  {
    id: "00000000-0000-4000-8000-000000000005",
    name: "dev-codex",
    description: "Local ProcessBackend smoke profile for Codex",
    icon: "Bot",
    harness: "codex",
  },
];

for (const seeded of profiles) {
  const values = { ...commonProfile, ...seeded };
  await db
    .insert(profile)
    .values(values)
    .onConflictDoUpdate({
      target: profile.id,
      set: values,
    });
}

console.log(
  `process-dev-bootstrap: ${EMAIL} is an admin; seeded ${profiles.map((p) => p.name).join(", ")}`,
);
process.exit(0);
