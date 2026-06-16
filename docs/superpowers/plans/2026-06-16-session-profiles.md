# Session Profiles Implementation Plan (ADR 0052)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the free-form new-session form with admin-curated *profiles*: each profile bundles an image, environment, and a Claude-token capability grant; users pick one and type a task.

**Architecture:** Profiles are **orchestrator-only** data (the control plane never learns about them). A new orchestrator-native `ProfileService` (CASL self-gated, like `TaskService`) does CRUD over a `profile` table in the orchestrator's Postgres. `CreateTask` drops `image_uri` and gains `profile_id`: it loads the profile, resolves `image_id` → current `image_uri` via the coordinator's `ImageService`, assembles `harness_env`, and records `profile_id` on `task_session`. The web new-session dialog becomes a searchable profile picker; a new admin surface under Settings does CRUD; a `ProfileSnapshot` embedded on `TaskSessionRef` drives an app-wide profile chip.

**Tech Stack:** proto3 + buf (`just gen-proto` → connect-es/connect-query) · orchestrator: TypeScript on Bun, Hono, Connect, Drizzle (Postgres), CASL · web: React, TanStack Router/Query, connect-query, shadcn/ui (lucide icons) · tests: `bun:test` (orchestrator), `vitest` (web).

**Delivery:** Single branch (`adr-0052-session-profiles`), **one commit at the end of every task** (per the exact commit message in each task's final step). Not stacked PRs.

**Source-grounding decisions (verified against the tree, 2026-06-16):**
- Profile `id` is `text` (uuid string via `crypto.randomUUID()`), matching the existing `task.id` text-id convention — **not** a pg `uuid` column. (ADR §1 says "uuid"; we store the uuid as text for consistency with `task`.)
- `CreateTask` profile-load and image-resolution go through **injectable seams** (`ProfileStore`, `ImagesClient`) mirroring the existing `secrets`/`sessions` deps, so the handler stays unit-testable without a DB.
- The `DisableImage` profile-guard is a **generic pre-flight hook** added to `registerPassthrough` (not a second `router.service(ImageService,…)` registration, which would double-register the service). `DisableImage` takes `image_uri`, so the guard resolves it → `enabled_images.id` via `ImageService.ListEnabledImages`, then checks `profile.image_id`.
- The `ProfileSnapshot` lives on `TaskSessionRef` (native, fully resolvable). `SessionDetail` (the only passthrough-`getSession` surface) reads the snapshot from the warm `useTasks()` cache by session id — no control-plane enrichment.

---

## File Structure

**Proto (`crates/engram-protocol/proto/engram/app/v1/`):**
- Create `profile.proto` — `ProfileService` + `Profile`/`ProfileSnapshot` + request/response messages. (TS-only; never added to `build.rs`.)
- Modify `task.proto` — import `profile.proto`; `CreateTaskRequest` drops `image_uri` (reserve 2), adds `profile_id=5`; `TaskSessionRef` adds `optional ProfileSnapshot profile=4`.

**Orchestrator (`orchestrator/src/`):**
- Modify `db/schema.ts` — add `profile` table + `taskSession.profileId`.
- Create `drizzle/0003_*.sql` — generated migration.
- Create `db/profiles.ts` — `ProfileStore` seam + `makeProfileStore` (Drizzle-backed).
- Modify `authz/ability.ts` — add `"Profile"` subject.
- Create `rpc/profiles.ts` — native `ProfileService` (`registerProfiles`).
- Modify `rpc/tasks.ts` — `CreateTask` rewrite (profile load, image resolve, token-gated `harness_env`, `profileId`); snapshot enrichment in `buildTask`/`loadTask`/`listTasks`; add `profiles`/`images` to `TaskDeps`.
- Create `rpc/image-guard.ts` — `makeDisableImageGuard` pre-flight.
- Modify `rpc/passthrough.ts` — add optional `preflight` param.
- Modify `index.ts` — register `ProfileService`; wire the `DisableImage` pre-flight.
- Tests: `__tests__/profiles.test.ts` (new); extend `__tests__/tasks.test.ts`; `__tests__/image-guard.test.ts` (new).

**Web (`web/src/`):**
- Add shadcn `popover`, `hover-card`, `switch` under `components/ui/`.
- Modify `lib/ability.ts` — add `"Profile"` subject.
- Modify `lib/types.ts` — add `profile?` to `SessionListItem`.
- Create `hooks/useProfiles.ts` — query/mutation hooks.
- Modify `components/NewSessionDialog.tsx` — profile picker.
- Modify `router.tsx` + `pages/settings/SettingsLayout.tsx` — admin routes + nav.
- Create `pages/settings/SessionProfiles.tsx` (list), `pages/settings/SessionProfileEditor.tsx` (create/edit).
- Create `components/profiles/ProfileChip.tsx`, `components/profiles/IconPicker.tsx`, `components/profiles/EnvVarsEditor.tsx`.
- Modify `hooks/useTasks.ts` (`taskToSessionListItem`), the sessions list row, the rail, and `SessionDetail` header — render `<ProfileChip>`.
- Modify the Operator → Images disable path — surface the `failed_precondition` message.

---

## Phase 0 — Proto contract

### Task 1: Add `profile.proto`, change `CreateTaskRequest`, extend `TaskSessionRef`

**Files:**
- Create: `crates/engram-protocol/proto/engram/app/v1/profile.proto`
- Modify: `crates/engram-protocol/proto/engram/app/v1/task.proto`
- Regenerates: `web/src/gen/**`, `orchestrator/src/gen/**` (via `just gen-proto`)

- [ ] **Step 1: Write `profile.proto`**

Create `crates/engram-protocol/proto/engram/app/v1/profile.proto`:

```proto
syntax = "proto3";

package engram.app.v1;

// Session profiles (ADR 0052) — admin-curated session starting points.
// ORCHESTRATOR-NATIVE: the control plane neither implements nor knows about
// profiles. This file lives in the app package so web gets one uniform
// generated API, and it is deliberately ABSENT from crates/engram-protocol/
// build.rs (never compiled into Rust), exactly like task.proto.
service ProfileService {
  // active only by default; include_archived is admin-only (ADR §4/§6).
  rpc ListProfiles(ListProfilesRequest) returns (ListProfilesResponse);
  rpc GetProfile(GetProfileRequest) returns (GetProfileResponse);
  rpc CreateProfile(CreateProfileRequest) returns (CreateProfileResponse);
  rpc UpdateProfile(UpdateProfileRequest) returns (UpdateProfileResponse);
  // Soft delete — sets deleted_at (ADR §4).
  rpc DeleteProfile(DeleteProfileRequest) returns (DeleteProfileResponse);
}

// An admin-curated bundle of session-launch inputs (ADR §1).
message Profile {
  string id = 1;
  string name = 2;
  string description = 3;
  // A lucide-react icon name (ADR §1; the web app's icon set).
  string icon = 4;
  // Logical ref to the coordinator's enabled_images.id (ADR §3) — NOT a wire
  // FK. Resolved to the live image_uri at session-create / read time.
  string image_id = 5;
  // Capability grant: inject the user's Claude token (ADR §5).
  bool include_user_tokens = 6;
  // Admin-only environment ({KEY: VALUE}); the model lives here. OMITTED
  // (empty) for non-admin callers (ADR §6).
  map<string, string> env_vars = 7;
  // deleted_at IS NOT NULL (ADR §4). Archived profiles are admin-visible only.
  bool archived = 8;
  // ISO-8601.
  string created_at = 9;
  // ISO-8601.
  string updated_at = 10;
}

// Lightweight resolved identity embedded on TaskSessionRef for the chip
// (ADR §9). image_uri is resolved from image_id at read time.
message ProfileSnapshot {
  string id = 1;
  string name = 2;
  string icon = 3;
  bool archived = 4;
  string image_uri = 5;
}

message ListProfilesRequest {
  // Admin-only; silently ignored (treated as false) for non-admin callers.
  bool include_archived = 1;
}

message ListProfilesResponse {
  repeated Profile profiles = 1;
}

message GetProfileRequest {
  string id = 1;
}

message GetProfileResponse {
  Profile profile = 1;
}

message CreateProfileRequest {
  string name = 1;
  string description = 2;
  string icon = 3;
  string image_id = 4;
  bool include_user_tokens = 5;
  map<string, string> env_vars = 6;
}

message CreateProfileResponse {
  Profile profile = 1;
}

message UpdateProfileRequest {
  string id = 1;
  string name = 2;
  string description = 3;
  string icon = 4;
  string image_id = 5;
  bool include_user_tokens = 6;
  map<string, string> env_vars = 7;
}

message UpdateProfileResponse {
  Profile profile = 1;
}

message DeleteProfileRequest {
  string id = 1;
}

message DeleteProfileResponse {}
```

- [ ] **Step 2: Edit `task.proto`** — import profile.proto, change `CreateTaskRequest`, extend `TaskSessionRef`.

Add the import after the existing session import (top of file):

```proto
import "engram/app/v1/session.proto";
import "engram/app/v1/profile.proto";
```

Replace the `CreateTaskRequest` message:

```proto
message CreateTaskRequest {
  // "chat" is the only accepted value for now;
  // linear_issue/dependabot/incident later.
  string type = 1;
  // ADR 0052: sessions now start from a profile, not a raw image. The image
  // is resolved server-side from the profile's image_id.
  reserved 2;
  reserved "image_uri";
  // The user's task (no longer ever an admin-configured value; profiles carry
  // no prompt — ADR §5).
  optional string prompt = 3;
  optional string title = 4;
  // The profile the user picked (ADR §5). Required in practice; the handler
  // rejects an empty/unknown/archived id.
  string profile_id = 5;
}
```

Add `profile` to `TaskSessionRef` (after the `session` field):

```proto
message TaskSessionRef {
  string session_id = 1;
  optional string role = 2;
  optional Session session = 3;
  // ADR 0052: resolved profile identity for the session's originating profile,
  // for the app-wide chip (§9). Unset for legacy / profile-less sessions.
  optional ProfileSnapshot profile = 4;
}
```

- [ ] **Step 3: Regenerate bindings**

Run: `just gen-proto`
Expected: succeeds; creates `web/src/gen/engram/app/v1/profile_pb.ts`, `web/src/gen/engram/app/v1/profile-ProfileService_connectquery.ts`, `orchestrator/src/gen/engram/app/v1/profile_pb.ts`, and updates `task_pb.ts` in both gen dirs. No errors.

- [ ] **Step 4: Verify lint + breaking locally**

Run: `buf lint crates/engram-protocol/proto`
Expected: clean (no findings on `profile.proto`/`task.proto`).

Run: `buf build crates/engram-protocol/proto`
Expected: succeeds (the `task.proto` → `profile.proto` import resolves).

Note: the orchestrator/web won't typecheck yet — they still reference the old `imageUri`. That's fixed in Tasks 2–7 / 10–11. Do not run app typecheck here.

- [ ] **Step 5: Commit**

```bash
git add crates/engram-protocol/proto/engram/app/v1/profile.proto \
        crates/engram-protocol/proto/engram/app/v1/task.proto \
        web/src/gen orchestrator/src/gen
git commit -m "$(cat <<'EOF'
feat(proto): add ProfileService; CreateTask takes profile_id (ADR 0052)

New profile.proto (orchestrator-native, TS-only — absent from build.rs).
CreateTaskRequest drops image_uri (reserved 2) and gains profile_id;
TaskSessionRef gains an optional ProfileSnapshot for the app-wide chip.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase 1 — Orchestrator data + ProfileService

### Task 2: `profile` table + `task_session.profile_id` + migration

**Files:**
- Modify: `orchestrator/src/db/schema.ts`
- Create: `orchestrator/drizzle/0003_session_profiles.sql` (generated; rename if drizzle picks another suffix)
- Test: `orchestrator/src/__tests__/db.test.ts` (extend) — DB-gated round-trip

- [ ] **Step 1: Add the schema** — in `orchestrator/src/db/schema.ts`, after the `taskSession` table (before the better-auth section), add:

```ts
// ---------------------------------------------------------------------------
// Session profiles (ADR 0052)
//
// Admin-curated session starting points. Orchestrator-only data — the control
// plane never learns about profiles. `image_id` is a LOGICAL ref to the
// coordinator's enabled_images.id (not a DB FK — different tier, ADR §3);
// integrity is enforced in application code. Soft delete only (deleted_at).
// ---------------------------------------------------------------------------

export const profile = pgTable("profile", {
  id: text("id").primaryKey(), // uuid string (crypto.randomUUID())
  name: text("name").notNull(),
  description: text("description").notNull().default(""),
  icon: text("icon").notNull().default("Bot"), // lucide icon name
  imageId: text("image_id").notNull(), // logical ref → enabled_images.id (§3)
  includeUserTokens: boolean("include_user_tokens").notNull().default(false),
  envVars: jsonb("env_vars").notNull().default({}), // { KEY: VALUE }
  createdAt: timestamp("created_at").notNull().defaultNow(),
  updatedAt: timestamp("updated_at")
    .notNull()
    .defaultNow()
    .$onUpdate(() => new Date()),
  deletedAt: timestamp("deleted_at"), // null = active; soft delete only (§4)
});
```

Then add `profileId` to the `taskSession` table column block (after `role`):

```ts
    role: text("role"), // nullable until multi-session types exist
    // ADR 0052: which profile started this session. Real intra-DB FK (§2).
    // Nullable for pre-feature / out-of-band sessions. Profiles are only ever
    // soft-deleted, so the target always exists; ON DELETE is moot.
    profileId: text("profile_id").references(() => profile.id),
    createdAt: timestamp("created_at").notNull().defaultNow(),
```

- [ ] **Step 2: Generate the migration**

Run: `cd orchestrator && bunx drizzle-kit generate --name session_profiles`
Expected: a new file `orchestrator/drizzle/0003_session_profiles.sql` containing `CREATE TABLE "profile"`, `ALTER TABLE "task_session" ADD COLUMN "profile_id"`, and a FK constraint `task_session_profile_id_profile_id_fk`.

Verify the SQL looks like (drizzle wording may vary slightly):

```sql
CREATE TABLE "profile" (
	"id" text PRIMARY KEY NOT NULL,
	"name" text NOT NULL,
	"description" text DEFAULT '' NOT NULL,
	"icon" text DEFAULT 'Bot' NOT NULL,
	"image_id" text NOT NULL,
	"include_user_tokens" boolean DEFAULT false NOT NULL,
	"env_vars" jsonb DEFAULT '{}'::jsonb NOT NULL,
	"created_at" timestamp DEFAULT now() NOT NULL,
	"updated_at" timestamp DEFAULT now() NOT NULL,
	"deleted_at" timestamp
);
--> statement-breakpoint
ALTER TABLE "task_session" ADD COLUMN "profile_id" text;--> statement-breakpoint
ALTER TABLE "task_session" ADD CONSTRAINT "task_session_profile_id_profile_id_fk" FOREIGN KEY ("profile_id") REFERENCES "public"."profile"("id") ON DELETE no action ON UPDATE no action;
```

- [ ] **Step 3: Apply the migration to the dev DB**

Run: `cd orchestrator && ORCHESTRATOR_DATABASE_URL="$ORCHESTRATOR_DATABASE_URL" bunx drizzle-kit migrate`
Expected: applies `0003_session_profiles`. (Requires the dev Postgres up — `just db-up` if needed. This is the same path the helm migrate-job runs in prod.)

- [ ] **Step 4: Write a DB-gated round-trip test** — append to `orchestrator/src/__tests__/db.test.ts` (it already has a DB gate). Add:

```ts
import { profile as profileTable, taskSession as taskSessionTable, task as taskTable } from "../db/schema.ts";

describe("profile table (ADR 0052)", () => {
  test.skipIf(!dbReachable)("insert profile + task_session.profile_id round-trips", async () => {
    const db = getDb();
    const pid = `profile-rt-${Date.now()}`;
    const tid = `profile-rt-task-${Date.now()}`;
    const sid = `profile-rt-sess-${Date.now()}`;
    try {
      await db.insert(profileTable).values({
        id: pid,
        name: "Round-trip",
        description: "d",
        icon: "Bot",
        imageId: "img-logical-id",
        includeUserTokens: true,
        envVars: { ANTHROPIC_MODEL: "claude-opus-4-8" },
      });
      await db.insert(taskTable).values({ id: tid, type: "chat", status: "open", createdByUserId: "u", source: {} });
      await db.insert(taskSessionTable).values({ taskId: tid, sessionId: sid, role: "primary", profileId: pid });

      const rows = await db.select().from(profileTable).where(eq(profileTable.id, pid));
      expect(rows[0]!.includeUserTokens).toBe(true);
      expect(rows[0]!.envVars).toEqual({ ANTHROPIC_MODEL: "claude-opus-4-8" });
      expect(rows[0]!.deletedAt).toBeNull();

      const refs = await db.select().from(taskSessionTable).where(eq(taskSessionTable.taskId, tid));
      expect(refs[0]!.profileId).toBe(pid);
    } finally {
      await db.delete(taskTable).where(eq(taskTable.id, tid)).catch(() => {});
      await db.delete(profileTable).where(eq(profileTable.id, pid)).catch(() => {});
    }
  });
});
```

(Ensure `eq` and `describe/test/expect` are imported at the top of `db.test.ts` — they already are in that file; add the schema imports if absent.)

- [ ] **Step 5: Run the test**

Run: `cd orchestrator && ORCHESTRATOR_DATABASE_URL="$ORCHESTRATOR_DATABASE_URL" bun test src/__tests__/db.test.ts`
Expected: PASS (or skipped if no DB — then run with the dev DB to confirm before committing).

- [ ] **Step 6: Commit**

```bash
git add orchestrator/src/db/schema.ts orchestrator/drizzle/ orchestrator/src/__tests__/db.test.ts
git commit -m "$(cat <<'EOF'
feat(orchestrator): profile table + task_session.profile_id (ADR 0052)

Drizzle migration 0003 adds the orchestrator-only profile table (soft
delete via deleted_at; image_id is a logical ref, not a DB FK) and a
nullable task_session.profile_id intra-DB FK recording which profile
started each session.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: `ProfileStore` data-access seam

**Files:**
- Create: `orchestrator/src/db/profiles.ts`
- Test: `orchestrator/src/__tests__/profiles-store.test.ts` (DB-gated)

- [ ] **Step 1: Write the store** — create `orchestrator/src/db/profiles.ts`:

```ts
/**
 * Profile data-access seam (ADR 0052).
 *
 * The injectable seam that ProfileService (rpc/profiles.ts) and
 * TaskService.createTask (rpc/tasks.ts) depend on, and that the seam tests
 * fake. Drizzle-backed by default. Soft delete only (deleted_at).
 */

import { and, eq, inArray, isNull } from "drizzle-orm";

import { getDb } from "./client.ts";
import { profile as profileTable } from "./schema.ts";

export interface ProfileRow {
  id: string;
  name: string;
  description: string;
  icon: string;
  imageId: string;
  includeUserTokens: boolean;
  envVars: Record<string, string>;
  createdAt: Date;
  updatedAt: Date;
  deletedAt: Date | null;
}

export interface ProfileInput {
  name: string;
  description: string;
  icon: string;
  imageId: string;
  includeUserTokens: boolean;
  envVars: Record<string, string>;
}

/** The seam injected into ProfileService and TaskService. */
export interface ProfileStore {
  /** Active profiles by default; includeArchived adds soft-deleted ones. Ordered by name. */
  list(opts: { includeArchived: boolean }): Promise<ProfileRow[]>;
  /** Any profile (active or archived), or null. */
  get(id: string): Promise<ProfileRow | null>;
  /** Active (deleted_at IS NULL) only, or null. Used by createTask. */
  getActive(id: string): Promise<ProfileRow | null>;
  /** Rows for the given ids (active or archived) — for snapshot enrichment. */
  getByIds(ids: string[]): Promise<ProfileRow[]>;
  create(input: ProfileInput): Promise<ProfileRow>;
  /** Returns the updated row, or null if the id is absent / archived. */
  update(id: string, input: ProfileInput): Promise<ProfileRow | null>;
  /** Idempotent soft delete (sets deleted_at). */
  softDelete(id: string): Promise<void>;
}

function toRow(r: typeof profileTable.$inferSelect): ProfileRow {
  return {
    id: r.id,
    name: r.name,
    description: r.description,
    icon: r.icon,
    imageId: r.imageId,
    includeUserTokens: r.includeUserTokens,
    envVars: (r.envVars ?? {}) as Record<string, string>,
    createdAt: r.createdAt,
    updatedAt: r.updatedAt,
    deletedAt: r.deletedAt,
  };
}

export function makeProfileStore(db: ReturnType<typeof getDb> = getDb()): ProfileStore {
  return {
    async list({ includeArchived }) {
      const rows = includeArchived
        ? await db.select().from(profileTable)
        : await db.select().from(profileTable).where(isNull(profileTable.deletedAt));
      return rows.map(toRow).sort((a, b) => a.name.localeCompare(b.name));
    },
    async get(id) {
      const rows = await db.select().from(profileTable).where(eq(profileTable.id, id)).limit(1);
      return rows[0] ? toRow(rows[0]) : null;
    },
    async getActive(id) {
      const rows = await db
        .select()
        .from(profileTable)
        .where(and(eq(profileTable.id, id), isNull(profileTable.deletedAt)))
        .limit(1);
      return rows[0] ? toRow(rows[0]) : null;
    },
    async getByIds(ids) {
      if (ids.length === 0) return [];
      const rows = await db.select().from(profileTable).where(inArray(profileTable.id, ids));
      return rows.map(toRow);
    },
    async create(input) {
      const id = crypto.randomUUID();
      await db.insert(profileTable).values({ id, ...input });
      const row = await this.get(id);
      return row!;
    },
    async update(id, input) {
      const existing = await this.getActive(id);
      if (!existing) return null;
      await db
        .update(profileTable)
        .set({ ...input, updatedAt: new Date() })
        .where(eq(profileTable.id, id));
      return this.get(id);
    },
    async softDelete(id) {
      await db
        .update(profileTable)
        .set({ deletedAt: new Date() })
        .where(and(eq(profileTable.id, id), isNull(profileTable.deletedAt)));
    },
  };
}
```

- [ ] **Step 2: Write the DB-gated test** — create `orchestrator/src/__tests__/profiles-store.test.ts`:

```ts
import { expect, test, describe } from "bun:test";
import { checkDb, getDb } from "../db/client.ts";
import { makeProfileStore } from "../db/profiles.ts";
import { profile as profileTable } from "../db/schema.ts";
import { eq } from "drizzle-orm";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;

describe("ProfileStore", () => {
  test.skipIf(!dbReachable)("create → getActive → list → update → softDelete", async () => {
    const store = makeProfileStore(getDb());
    const input = {
      name: `Store Test ${Date.now()}`,
      description: "d",
      icon: "Bot",
      imageId: "img-1",
      includeUserTokens: false,
      envVars: { ANTHROPIC_MODEL: "claude-opus-4-8" },
    };
    const created = await store.create(input);
    try {
      expect(created.id).toBeDefined();
      expect(created.deletedAt).toBeNull();

      const active = await store.getActive(created.id);
      expect(active?.name).toBe(input.name);

      const listed = await store.list({ includeArchived: false });
      expect(listed.some((p) => p.id === created.id)).toBe(true);

      const updated = await store.update(created.id, { ...input, includeUserTokens: true });
      expect(updated?.includeUserTokens).toBe(true);

      await store.softDelete(created.id);
      expect(await store.getActive(created.id)).toBeNull();
      // Still resolvable via get() (history) and getByIds().
      expect((await store.get(created.id))?.deletedAt).not.toBeNull();
      const byIds = await store.getByIds([created.id]);
      expect(byIds[0]?.deletedAt).not.toBeNull();
      // Archived hidden from default list, shown with includeArchived.
      expect((await store.list({ includeArchived: false })).some((p) => p.id === created.id)).toBe(false);
      expect((await store.list({ includeArchived: true })).some((p) => p.id === created.id)).toBe(true);

      // update on an archived id → null.
      expect(await store.update(created.id, input)).toBeNull();
    } finally {
      await getDb().delete(profileTable).where(eq(profileTable.id, created.id)).catch(() => {});
    }
  });
});
```

- [ ] **Step 3: Run**

Run: `cd orchestrator && ORCHESTRATOR_DATABASE_URL="$ORCHESTRATOR_DATABASE_URL" bun test src/__tests__/profiles-store.test.ts`
Expected: PASS against the dev DB.

- [ ] **Step 4: Commit**

```bash
git add orchestrator/src/db/profiles.ts orchestrator/src/__tests__/profiles-store.test.ts
git commit -m "$(cat <<'EOF'
feat(orchestrator): ProfileStore data-access seam (ADR 0052)

Injectable Drizzle-backed store for profile CRUD with soft-delete
semantics (getActive vs get, list includeArchived). The seam both
ProfileService and createTask depend on and tests fake.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: `"Profile"` CASL subject (orchestrator)

**Files:**
- Modify: `orchestrator/src/authz/ability.ts`
- Test: `orchestrator/src/__tests__/authz.matrix.test.ts` (extend) — or a focused new test if the matrix file is awkward

- [ ] **Step 1: Write the failing test** — append to `orchestrator/src/__tests__/authz.matrix.test.ts` a focused block (adjust imports to match that file's existing `abilityFor` import):

```ts
import { abilityFor } from "../authz/ability.ts";

describe("Profile subject (ADR 0052)", () => {
  test("member can read Profile but not manage", () => {
    const a = abilityFor({ id: "m", role: "user" });
    expect(a.can("read", "Profile")).toBe(true);
    expect(a.can("manage", "Profile")).toBe(false);
    expect(a.can("create", "Profile")).toBe(false);
    expect(a.can("delete", "Profile")).toBe(false);
  });
  test("admin can manage Profile", () => {
    const a = abilityFor({ id: "x", role: "admin" });
    expect(a.can("read", "Profile")).toBe(true);
    expect(a.can("manage", "Profile")).toBe(true);
    expect(a.can("create", "Profile")).toBe(true);
    expect(a.can("delete", "Profile")).toBe(true);
  });
});
```

- [ ] **Step 2: Run — expect fail**

Run: `cd orchestrator && bun test src/__tests__/authz.matrix.test.ts`
Expected: FAIL — `read Profile` is false for the member (the subject doesn't exist yet, CASL returns false).

- [ ] **Step 3: Add the subject** — in `orchestrator/src/authz/ability.ts`:

Extend the `Subjects` union:

```ts
export type Subjects =
  | "Task"
  | "Session"
  | "EnabledImage"
  | "Profile"
  | "Fleet"
  | "Registry"
  | "all";
```

In `abilityFor`, after the `can("read", "EnabledImage");` line, add:

```ts
  // Profiles: the menu every member picks from is readable; mutations are
  // admin-only (covered by manage("all") below). ADR 0052 §6.
  can("read", "Profile");
```

(No explicit admin line needed — `manage("all")` already covers `manage Profile`.)

- [ ] **Step 4: Run — expect pass**

Run: `cd orchestrator && bun test src/__tests__/authz.matrix.test.ts`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add orchestrator/src/authz/ability.ts orchestrator/src/__tests__/authz.matrix.test.ts
git commit -m "$(cat <<'EOF'
feat(orchestrator): add Profile CASL subject (ADR 0052)

Members can read Profile (the picker menu); mutations stay admin-only
via manage(all).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Native `ProfileService`

**Files:**
- Create: `orchestrator/src/rpc/profiles.ts`
- Modify: `orchestrator/src/index.ts` (register it)
- Test: `orchestrator/src/__tests__/profiles.test.ts`

- [ ] **Step 1: Write the handler** — create `orchestrator/src/rpc/profiles.ts`:

```ts
/**
 * Native ProfileService implementation (ADR 0052).
 *
 * Orchestrator-native (like TaskService): registered on the ConnectRouter,
 * NEVER proxied. Self-gates with the same CASL machinery as everything else.
 *
 * Field-level filtering (ADR §6): non-admin callers get env_vars stripped;
 * include_archived is admin-only. Mutations are admin-only.
 *
 * Image integrity (ADR §3): create/update validate image_id against the
 * coordinator's enabled-image catalog and reject an absent/disabled id.
 *
 * Injectable deps (getSession, store, images) for tests.
 */

import { ConnectError, Code } from "@connectrpc/connect";
import type { ConnectRouter, HandlerContext } from "@connectrpc/connect";

import { ProfileService } from "../gen/engram/app/v1/profile_pb.ts";
import type { Profile } from "../gen/engram/app/v1/profile_pb.ts";

import { abilityFor } from "../authz/ability.ts";
import { auth } from "../auth/better-auth.ts";
import { getDb } from "../db/client.ts";
import { makeProfileStore, type ProfileRow, type ProfileStore } from "../db/profiles.ts";
import { images as defaultImages } from "../control-plane/client.ts";

/** Subset of ImageService client used here (catalog validation). */
export interface ImagesClient {
  listEnabledImages(req: Record<string, never>): Promise<{
    images: Array<{ id: string; imageUri: string }>;
  }>;
}

export type GetSession = (
  headers: Headers,
) => Promise<{ user: { id: string; role?: string | null; email?: string | null } } | null>;

export interface ProfileDeps {
  getSession?: GetSession;
  store?: ProfileStore;
  images?: ImagesClient;
}

function headersOf(ctx: HandlerContext): Headers {
  return ctx.requestHeader;
}

async function requireUser(ctx: HandlerContext, getSession: GetSession): Promise<{ id: string; role: string }> {
  const session = await getSession(headersOf(ctx));
  if (!session) throw new ConnectError("unauthenticated", Code.Unauthenticated);
  return { id: session.user.id, role: session.user.role ?? "user" };
}

/** Map a ProfileRow to the proto Profile. env_vars included only when admin. */
function toProto(row: ProfileRow, isAdmin: boolean): Profile {
  return {
    id: row.id,
    name: row.name,
    description: row.description,
    icon: row.icon,
    imageId: row.imageId,
    includeUserTokens: row.includeUserTokens,
    envVars: isAdmin ? row.envVars : {},
    archived: row.deletedAt != null,
    createdAt: row.createdAt.toISOString(),
    updatedAt: row.updatedAt.toISOString(),
  } as Profile;
}

export function registerProfiles(router: ConnectRouter, deps?: ProfileDeps): void {
  const getSession: GetSession =
    deps?.getSession ??
    ((headers) => auth.api.getSession({ headers } as Parameters<typeof auth.api.getSession>[0]));
  const store: ProfileStore = deps?.store ?? makeProfileStore(getDb());
  const images: ImagesClient = deps?.images ?? (defaultImages as unknown as ImagesClient);

  /** Validate image_id against the live catalog; throw InvalidArgument if absent. */
  async function assertImageEnabled(imageId: string): Promise<void> {
    const resp = await images.listEnabledImages({});
    if (!resp.images.some((i) => i.id === imageId)) {
      throw new ConnectError("image_id is not an enabled image", Code.InvalidArgument);
    }
  }

  router.service(ProfileService, {
    async listProfiles(req, ctx) {
      const user = await requireUser(ctx, getSession);
      const ability = abilityFor(user);
      if (!ability.can("read", "Profile")) throw new ConnectError("forbidden", Code.PermissionDenied);
      const isAdmin = user.role === "admin";
      // include_archived is admin-only; silently forced false for members.
      const includeArchived = isAdmin && req.includeArchived;
      const rows = await store.list({ includeArchived });
      return { profiles: rows.map((r) => toProto(r, isAdmin)) };
    },

    async getProfile(req, ctx) {
      const user = await requireUser(ctx, getSession);
      const ability = abilityFor(user);
      if (!ability.can("read", "Profile")) throw new ConnectError("forbidden", Code.PermissionDenied);
      const isAdmin = user.role === "admin";
      const row = await store.get(req.id);
      // Members never see archived profiles.
      if (!row || (row.deletedAt != null && !isAdmin)) {
        throw new ConnectError("not found", Code.NotFound);
      }
      return { profile: toProto(row, isAdmin) };
    },

    async createProfile(req, ctx) {
      const user = await requireUser(ctx, getSession);
      const ability = abilityFor(user);
      if (!ability.can("manage", "Profile")) throw new ConnectError("forbidden", Code.PermissionDenied);
      if (!req.name.trim()) throw new ConnectError("name is required", Code.InvalidArgument);
      await assertImageEnabled(req.imageId);
      const row = await store.create({
        name: req.name,
        description: req.description,
        icon: req.icon || "Bot",
        imageId: req.imageId,
        includeUserTokens: req.includeUserTokens,
        envVars: req.envVars ?? {},
      });
      return { profile: toProto(row, true) };
    },

    async updateProfile(req, ctx) {
      const user = await requireUser(ctx, getSession);
      const ability = abilityFor(user);
      if (!ability.can("manage", "Profile")) throw new ConnectError("forbidden", Code.PermissionDenied);
      if (!req.name.trim()) throw new ConnectError("name is required", Code.InvalidArgument);
      await assertImageEnabled(req.imageId);
      const row = await store.update(req.id, {
        name: req.name,
        description: req.description,
        icon: req.icon || "Bot",
        imageId: req.imageId,
        includeUserTokens: req.includeUserTokens,
        envVars: req.envVars ?? {},
      });
      if (!row) throw new ConnectError("not found", Code.NotFound);
      return { profile: toProto(row, true) };
    },

    async deleteProfile(req, ctx) {
      const user = await requireUser(ctx, getSession);
      const ability = abilityFor(user);
      if (!ability.can("manage", "Profile")) throw new ConnectError("forbidden", Code.PermissionDenied);
      await store.softDelete(req.id);
      return {};
    },
  });
}
```

- [ ] **Step 2: Register in `index.ts`** — in `orchestrator/src/index.ts`, import and register before the passthrough:

Add the import beside `registerTasks`:

```ts
import { registerTasks } from "./rpc/tasks.ts";
import { registerProfiles } from "./rpc/profiles.ts";
```

In the router callback, after `registerTasks(router);`:

```ts
    registerTasks(router);
    // Native ProfileService: orchestrator-owned session profiles (ADR 0052).
    registerProfiles(router);
```

- [ ] **Step 3: Write the test** — create `orchestrator/src/__tests__/profiles.test.ts` (mirrors `tasks.test.ts` fake-deps + server-spawn idiom):

```ts
import { expect, test, describe, beforeAll, afterAll } from "bun:test";
import { ConnectError, Code, createClient } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-node";
import { Hono } from "hono";
import type { AddressInfo } from "node:net";

import { buildServer } from "../server.ts";
import { registerProfiles } from "../rpc/profiles.ts";
import type { ProfileDeps, ImagesClient, GetSession } from "../rpc/profiles.ts";
import type { ProfileRow, ProfileStore, ProfileInput } from "../db/profiles.ts";
import { ProfileService } from "../gen/engram/app/v1/profile_pb.ts";

function makeGetSession(userId: string | null, role: "user" | "admin" = "user"): GetSession {
  return async () => (userId ? { user: { id: userId, role, email: `${userId}@t.invalid` } } : null);
}

const fakeImages = (ids: string[]): ImagesClient => ({
  async listEnabledImages() {
    return { images: ids.map((id) => ({ id, imageUri: `registry/${id}:latest` })) };
  },
});

/** In-memory ProfileStore for the authz/field-filter matrix (no DB). */
function makeFakeStore(seed: ProfileRow[] = []): ProfileStore {
  const rows = new Map<string, ProfileRow>(seed.map((r) => [r.id, r]));
  let n = 0;
  const mk = (id: string, input: ProfileInput): ProfileRow => ({
    id, ...input, createdAt: new Date(0), updatedAt: new Date(0), deletedAt: null,
  });
  return {
    async list({ includeArchived }) {
      return [...rows.values()]
        .filter((r) => includeArchived || r.deletedAt == null)
        .sort((a, b) => a.name.localeCompare(b.name));
    },
    async get(id) { return rows.get(id) ?? null; },
    async getActive(id) { const r = rows.get(id); return r && r.deletedAt == null ? r : null; },
    async getByIds(ids) { return ids.map((i) => rows.get(i)).filter(Boolean) as ProfileRow[]; },
    async create(input) { const id = `p${n++}`; const r = mk(id, input); rows.set(id, r); return r; },
    async update(id, input) {
      const ex = rows.get(id); if (!ex || ex.deletedAt != null) return null;
      const r = { ...ex, ...input, updatedAt: new Date(0) }; rows.set(id, r); return r;
    },
    async softDelete(id) { const r = rows.get(id); if (r) rows.set(id, { ...r, deletedAt: new Date(0) }); },
  };
}

async function spawn(deps: ProfileDeps) {
  const app = new Hono();
  app.notFound((c) => c.json({ error: "not found" }, 404));
  const srv = buildServer(app, (router) => registerProfiles(router, deps));
  const url = await new Promise<string>((res) =>
    srv.listen(0, "127.0.0.1", () => res(`http://127.0.0.1:${(srv.address() as AddressInfo).port}`)),
  );
  return {
    client: createClient(ProfileService, createConnectTransport({ baseUrl: `${url}/rpc`, httpVersion: "1.1" })),
    close: () => new Promise<void>((res, rej) => srv.close((e) => (e ? rej(e) : res()))),
  };
}

async function expectErr(p: Promise<unknown>, code: Code) {
  try { await p; throw new Error(`expected ${Code[code]}`); }
  catch (e) { if (!(e instanceof ConnectError)) throw e; expect(e.code).toBe(code); }
}

const archived: ProfileRow = {
  id: "arch", name: "Archived", description: "", icon: "Bot", imageId: "img-1",
  includeUserTokens: false, envVars: { K: "V" }, createdAt: new Date(0), updatedAt: new Date(0),
  deletedAt: new Date(0),
};
const active: ProfileRow = { ...archived, id: "act", name: "Active", deletedAt: null };

describe("ProfileService — auth + field filtering", () => {
  test("anon ListProfiles → Unauthenticated", async () => {
    const s = await spawn({ getSession: makeGetSession(null), store: makeFakeStore(), images: fakeImages([]) });
    try { await expectErr(s.client.listProfiles({}), Code.Unauthenticated); } finally { await s.close(); }
  });

  test("member: active only, env_vars stripped, include_archived ignored", async () => {
    const s = await spawn({
      getSession: makeGetSession("m"), store: makeFakeStore([active, archived]), images: fakeImages(["img-1"]),
    });
    try {
      const r = await s.client.listProfiles({ includeArchived: true });
      expect(r.profiles.map((p) => p.id)).toEqual(["act"]); // archived hidden, include ignored
      expect(r.profiles[0]!.envVars).toEqual({}); // stripped
      expect(r.profiles[0]!.includeUserTokens).toBe(false); // still visible
    } finally { await s.close(); }
  });

  test("admin: include_archived shows archived + env_vars present", async () => {
    const s = await spawn({
      getSession: makeGetSession("a", "admin"), store: makeFakeStore([active, archived]), images: fakeImages(["img-1"]),
    });
    try {
      const r = await s.client.listProfiles({ includeArchived: true });
      expect(r.profiles.map((p) => p.id).sort()).toEqual(["act", "arch"]);
      expect(r.profiles.find((p) => p.id === "act")!.envVars).toEqual({ K: "V" });
    } finally { await s.close(); }
  });

  test("member CreateProfile → PermissionDenied", async () => {
    const s = await spawn({ getSession: makeGetSession("m"), store: makeFakeStore(), images: fakeImages(["img-1"]) });
    try {
      await expectErr(
        s.client.createProfile({ name: "x", description: "", icon: "Bot", imageId: "img-1", includeUserTokens: false, envVars: {} }),
        Code.PermissionDenied,
      );
    } finally { await s.close(); }
  });

  test("admin CreateProfile with unknown image_id → InvalidArgument", async () => {
    const s = await spawn({ getSession: makeGetSession("a", "admin"), store: makeFakeStore(), images: fakeImages(["img-1"]) });
    try {
      await expectErr(
        s.client.createProfile({ name: "x", description: "", icon: "Bot", imageId: "nope", includeUserTokens: false, envVars: {} }),
        Code.InvalidArgument,
      );
    } finally { await s.close(); }
  });

  test("admin CreateProfile happy path returns archived=false + env_vars", async () => {
    const s = await spawn({ getSession: makeGetSession("a", "admin"), store: makeFakeStore(), images: fakeImages(["img-1"]) });
    try {
      const r = await s.client.createProfile({
        name: "New", description: "d", icon: "Rocket", imageId: "img-1", includeUserTokens: true, envVars: { ANTHROPIC_MODEL: "claude-opus-4-8" },
      });
      expect(r.profile!.archived).toBe(false);
      expect(r.profile!.envVars).toEqual({ ANTHROPIC_MODEL: "claude-opus-4-8" });
      expect(r.profile!.includeUserTokens).toBe(true);
    } finally { await s.close(); }
  });
});
```

- [ ] **Step 4: Run**

Run: `cd orchestrator && bun test src/__tests__/profiles.test.ts`
Expected: PASS (no DB needed — fakes).

- [ ] **Step 5: Commit**

```bash
git add orchestrator/src/rpc/profiles.ts orchestrator/src/index.ts orchestrator/src/__tests__/profiles.test.ts
git commit -m "$(cat <<'EOF'
feat(orchestrator): native ProfileService CRUD (ADR 0052)

Five self-gated RPCs: members read active profiles (env_vars stripped,
include_archived ignored); admins manage and see archived + env_vars.
create/update validate image_id against the live image catalog.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase 2 — CreateTask rewrite, snapshot enrichment, DisableImage guard

### Task 6: `CreateTask` resolves a profile

**Files:**
- Modify: `orchestrator/src/rpc/tasks.ts`
- Test: `orchestrator/src/__tests__/tasks.test.ts` (rewrite create call sites + add precedence tests)

- [ ] **Step 1: Add deps + rewrite `createTask`** — in `orchestrator/src/rpc/tasks.ts`:

Add imports near the top:

```ts
import { makeProfileStore, type ProfileStore } from "../db/profiles.ts";
import { images as defaultImages } from "../control-plane/client.ts";
import { CLAUDE_OAUTH_ENV_VAR } from "../db/user-secrets.ts";
```

Add an `ImagesClient` interface (after `SessionsClient`):

```ts
/** Subset of ImageService client used by TaskService for image resolution. */
export interface ImagesClient {
  listEnabledImages(req: Record<string, never>): Promise<{
    images: Array<{ id: string; imageUri: string }>;
  }>;
}
```

Extend `TaskDeps`:

```ts
export interface TaskDeps {
  getSession?: GetSession;
  sessions?: SessionsClient;
  secrets?: UserSecretStore;
  profiles?: ProfileStore;
  images?: ImagesClient;
  db?: Db;
}
```

In `registerTasks`, resolve the new deps alongside the existing ones:

```ts
  const profiles: ProfileStore = deps?.profiles ?? makeProfileStore(getDbFn());
  const imagesClient: ImagesClient = deps?.images ?? (defaultImages as unknown as ImagesClient);
```

Replace the body of `createTask` (the secret-resolution block through the `createSession` call) with profile-driven resolution. The new handler:

```ts
    async createTask(req, ctx) {
      const user = await requireUser(ctx, getSession);
      const ability = abilityFor(user);

      if (req.type !== "chat") {
        throw new ConnectError("only chat tasks exist yet", Code.InvalidArgument);
      }
      if (!ability.can("create", "Task")) {
        throw new ConnectError("forbidden", Code.PermissionDenied);
      }
      if (!req.profileId) {
        throw new ConnectError("profile_id is required", Code.InvalidArgument);
      }

      // 1. Load the active profile (ADR §5.1). Missing/archived → rejected.
      const profile = await profiles.getActive(req.profileId);
      if (!profile) {
        throw new ConnectError("profile not found or archived", Code.NotFound);
      }

      // 2. Resolve image_id → current image_uri (ADR §5.2). Defense in depth
      //    behind the DisableImage guard (Task 8): reject if no longer enabled.
      const catalog = await imagesClient.listEnabledImages({});
      const image = catalog.images.find((i) => i.id === profile.imageId);
      if (!image) {
        throw new ConnectError(
          "the profile's image is no longer enabled — contact an admin",
          Code.FailedPrecondition,
        );
      }

      // 3. Assemble harness_env (ADR §5.4), lowest → highest precedence:
      //    user Claude token (only if include_user_tokens) < profile env_vars.
      //    NEVER log values.
      const harness: Record<string, string> = {};
      if (profile.includeUserTokens) {
        try {
          const userToken = await resolveSecrets().get(user.id, CLAUDE_OAUTH_ENV_VAR);
          if (userToken) harness[CLAUDE_OAUTH_ENV_VAR] = userToken;
        } catch (secretErr) {
          console.warn(
            `[TaskService] createTask: token lookup failed for user ${user.id} — booting without it`,
            secretErr,
          );
        }
      }
      for (const [k, v] of Object.entries(profile.envVars)) harness[k] = v; // profile overrides
      const harnessEnv = Object.keys(harness).length > 0 ? harness : undefined;

      // 4. Create the upstream session. Mode is always "agent" (ADR §5).
      const created = await sessionsClient.createSession({
        imageUri: image.imageUri,
        mode: "agent",
        ...(req.prompt != null ? { prompt: req.prompt } : {}),
        ...(harnessEnv != null ? { harnessEnv } : {}),
      });

      // 5. Insert task + task_session (recording profile_id). Compensate on failure.
      const taskId = crypto.randomUUID();
      try {
        const db = getDbFn();
        await db.transaction(async (tx) => {
          await tx.insert(taskTable).values({
            id: taskId,
            type: "chat",
            title: req.title ?? null,
            status: "open",
            createdByUserId: user.id,
            source: {},
          });
          await tx.insert(taskSessionTable).values({
            taskId,
            sessionId: created.sessionId,
            role: "primary",
            profileId: profile.id,
          });
        });
      } catch (dbErr) {
        try {
          await sessionsClient.deleteSession({ sessionId: created.sessionId });
        } catch (delErr) {
          console.error(
            `[TaskService] createTask compensation: failed to delete orphan session ${created.sessionId} after DB error`,
            delErr,
          );
        }
        throw dbErr;
      }

      evictOwnerCacheEntry(created.sessionId);
      const loaded = await loadTask(taskId, getDbFn(), sessionsClient, profiles, imagesClient);
      return { task: loaded };
    },
```

(Note: the old code injected the full secret map via `resolveSecrets().getAll`; the new behavior is the token-gated `.get(user.id, CLAUDE_OAUTH_ENV_VAR)` per ADR §5 — `include_user_tokens` is now the gate. `loadTask` gains `profiles`/`imagesClient` params — added in Task 7. Until Task 7 lands, temporarily call `loadTask(taskId, getDbFn(), sessionsClient)` and update the signature in Task 7; OR implement Task 7's `loadTask` change first. To keep each task green, do Task 7's `loadTask`/`buildTask` signature change as part of this step — see Task 7 Step 1, and apply it here too.)

> **Sequencing note:** Tasks 6 and 7 both touch `loadTask`/`buildTask`. Implement Task 7's signature changes in the same edit session so `tasks.ts` typechecks after Task 6. The commits stay separate (logic vs enrichment), but apply the `loadTask` signature in Task 7 Step 1 before running Task 6's tests.

- [ ] **Step 2: Rewrite the test call sites** — in `orchestrator/src/__tests__/tasks.test.ts`:

`createTask` no longer accepts `imageUri`; it needs `profileId`, and the handler loads a profile + resolves an image. Update the fakes:

(a) Add a fake `ProfileStore` + `ImagesClient` helper near the other fakes:

```ts
import type { ProfileRow, ProfileStore, ProfileInput } from "../db/profiles.ts";
import type { ImagesClient } from "../rpc/tasks.ts";

const PROFILE_ID = "test-profile";

function makeFakeProfiles(opts?: { includeUserTokens?: boolean; envVars?: Record<string, string>; imageId?: string }): ProfileStore {
  const row: ProfileRow = {
    id: PROFILE_ID, name: "Test", description: "", icon: "Bot",
    imageId: opts?.imageId ?? "img-1",
    includeUserTokens: opts?.includeUserTokens ?? false,
    envVars: opts?.envVars ?? {},
    createdAt: new Date(0), updatedAt: new Date(0), deletedAt: null,
  };
  const rows = new Map([[row.id, row]]);
  return {
    async list() { return [...rows.values()]; },
    async get(id) { return rows.get(id) ?? null; },
    async getActive(id) { const r = rows.get(id); return r && !r.deletedAt ? r : null; },
    async getByIds(ids) { return ids.map((i) => rows.get(i)).filter(Boolean) as ProfileRow[]; },
    async create(i: ProfileInput) { const r = { ...row, ...i }; rows.set(r.id, r); return r; },
    async update() { return null; },
    async softDelete() {},
  };
}

const fakeImages = (ids = ["img-1"]): ImagesClient => ({
  async listEnabledImages() { return { images: ids.map((id) => ({ id, imageUri: `registry/${id}:latest` })) }; },
});
```

(b) Add `profiles: makeFakeProfiles(), images: fakeImages()` to every `spawnServer({...})` deps bag.

(c) Replace every `client.createTask({ type: "chat", imageUri: "registry/img:latest", ... })` with `client.createTask({ type: "chat", profileId: PROFILE_ID, ... })`. The anon/type-validation tests pass `profileId: PROFILE_ID` (they fail before the profile is loaded, so the value is moot but must typecheck).

(d) Rewrite the harness-env tests (§6b) to gate on `include_user_tokens`:

```ts
test("include_user_tokens=true + token present → harness_env carries the token", async () => {
  const fakeSessions = makeFakeSessions({ created: [/* … */], existing: [] });
  const srv = await spawnServer({
    getSession: makeGetSession(MEMBER_A),
    sessions: fakeSessions,
    secrets: makeFakeTokens({ [MEMBER_A]: "sk-ant-oat01-secret" }),
    profiles: makeFakeProfiles({ includeUserTokens: true }),
    images: fakeImages(),
    db: okDb(),
  });
  try {
    const client = makeClient(srv.serverUrl);
    await client.createTask({ type: "chat", profileId: PROFILE_ID });
    expect(fakeSessions.createReqs[0]?.harnessEnv).toEqual({ CLAUDE_CODE_OAUTH_TOKEN: "sk-ant-oat01-secret" });
  } finally { await srv.close(); }
});

test("include_user_tokens=false → token NOT injected even when present", async () => {
  const fakeSessions = makeFakeSessions({ created: [/* … */], existing: [] });
  const srv = await spawnServer({
    getSession: makeGetSession(MEMBER_A),
    sessions: fakeSessions,
    secrets: makeFakeTokens({ [MEMBER_A]: "sk-ant-oat01-secret" }),
    profiles: makeFakeProfiles({ includeUserTokens: false }),
    images: fakeImages(),
    db: okDb(),
  });
  try {
    const client = makeClient(srv.serverUrl);
    await client.createTask({ type: "chat", profileId: PROFILE_ID });
    expect(fakeSessions.createReqs[0]?.harnessEnv).toBeUndefined();
  } finally { await srv.close(); }
});

test("profile env_vars override the user token key", async () => {
  const fakeSessions = makeFakeSessions({ created: [/* … */], existing: [] });
  const srv = await spawnServer({
    getSession: makeGetSession(MEMBER_A),
    sessions: fakeSessions,
    secrets: makeFakeTokens({ [MEMBER_A]: "user-token" }),
    profiles: makeFakeProfiles({ includeUserTokens: true, envVars: { CLAUDE_CODE_OAUTH_TOKEN: "admin-token", ANTHROPIC_MODEL: "claude-opus-4-8" } }),
    images: fakeImages(),
    db: okDb(),
  });
  try {
    const client = makeClient(srv.serverUrl);
    await client.createTask({ type: "chat", profileId: PROFILE_ID });
    expect(fakeSessions.createReqs[0]?.harnessEnv).toEqual({ CLAUDE_CODE_OAUTH_TOKEN: "admin-token", ANTHROPIC_MODEL: "claude-opus-4-8" });
  } finally { await srv.close(); }
});
```

(Fill the `created: [...]` FakeSession entries by copying the existing shape used elsewhere in the file.)

(e) Add a rejection test:

```ts
test("CreateTask with unknown profile_id → NotFound", async () => {
  const srv = await spawnServer({
    getSession: makeGetSession(MEMBER_A),
    sessions: makeFakeSessions({ existing: [] }),
    secrets: makeFakeTokens(),
    profiles: makeFakeProfiles(),
    images: fakeImages(),
    db: okDb(),
  });
  try {
    await expectConnectError(makeClient(srv.serverUrl).createTask({ type: "chat", profileId: "does-not-exist" }), Code.NotFound);
  } finally { await srv.close(); }
});
```

(f) The DB-gated CRUD test (`#4`) must insert a profile row before `createTask`, since the real `createTask` resolves it. In that `describe`'s `beforeAll`, insert a profile and pass `imageId` matching `fakeImages`:

```ts
// in beforeAll, after dbReachable guard:
await db!.insert(profileTable).values({
  id: PROFILE_ID, name: "CRUD", description: "", icon: "Bot",
  imageId: "img-1", includeUserTokens: false, envVars: {},
});
// pass profiles: makeProfileStore(db!), images: fakeImages() to spawnServer
// in afterAll: await db!.delete(profileTable).where(eq(profileTable.id, PROFILE_ID)).catch(()=>{})
```

(Import `profile as profileTable` and `makeProfileStore`.)

- [ ] **Step 3: Run**

Run: `cd orchestrator && ORCHESTRATOR_DATABASE_URL="$ORCHESTRATOR_DATABASE_URL" bun test src/__tests__/tasks.test.ts`
Expected: PASS (non-DB token-precedence + rejection tests always run; DB-gated lifecycle runs against the dev DB).

- [ ] **Step 4: Commit**

```bash
git add orchestrator/src/rpc/tasks.ts orchestrator/src/__tests__/tasks.test.ts
git commit -m "$(cat <<'EOF'
feat(orchestrator): CreateTask resolves a profile (ADR 0052)

createTask drops image_uri for profile_id: loads the active profile,
resolves image_id -> current image_uri, gates the Claude token on
include_user_tokens (profile env_vars override), sends mode=agent, and
records profile_id on task_session. include_user_tokens replaces the
old unconditional secret injection.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 7: Embed `ProfileSnapshot` on `TaskSessionRef`

**Files:**
- Modify: `orchestrator/src/rpc/tasks.ts` (`buildTask`, `loadTask`, `listTasks`)
- Test: `orchestrator/src/__tests__/tasks.test.ts` (extend)

- [ ] **Step 1: Thread profiles/images into the loaders** — in `orchestrator/src/rpc/tasks.ts`:

Change `buildTask` to accept the new profile/image maps and select `profileId` on refs. Update its signature and body:

```ts
function buildTask(
  row: { /* unchanged */ },
  sessionRefs: Array<{ sessionId: string; role: string | null; profileId: string | null }>,
  sessionMap: Map<string, Session>,
  profileMap: Map<string, { id: string; name: string; icon: string; archived: boolean; imageUri: string }>,
): Task {
  // … status derivation unchanged …
  const sessions: TaskSessionRef[] = sessionRefs.map((ref) => {
    const liveSession = sessionMap.get(ref.sessionId);
    const snap = ref.profileId != null ? profileMap.get(ref.profileId) : undefined;
    return {
      sessionId: ref.sessionId,
      ...(ref.role != null ? { role: ref.role } : {}),
      ...(liveSession != null ? { session: liveSession } : {}),
      ...(snap != null ? { profile: snap } : {}),
    } as TaskSessionRef;
  });
  // … return unchanged …
}
```

`buildUnattributedTask` passes no profile (orphan sessions have no task_session row) — leave it as-is (its ref has no `profileId`).

Add a helper that builds the profile map from a set of refs:

```ts
/** Resolve { profileId } → snapshot for the given refs (one images call + one profile query). */
async function buildProfileMap(
  refs: Array<{ profileId: string | null }>,
  profiles: ProfileStore,
  imagesClient: ImagesClient,
): Promise<Map<string, { id: string; name: string; icon: string; archived: boolean; imageUri: string }>> {
  const ids = [...new Set(refs.map((r) => r.profileId).filter((x): x is string => x != null))];
  const out = new Map<string, { id: string; name: string; icon: string; archived: boolean; imageUri: string }>();
  if (ids.length === 0) return out;
  const [rows, catalog] = await Promise.all([profiles.getByIds(ids), imagesClient.listEnabledImages({})]);
  const uriById = new Map(catalog.images.map((i) => [i.id, i.imageUri]));
  for (const p of rows) {
    out.set(p.id, {
      id: p.id, name: p.name, icon: p.icon,
      archived: p.deletedAt != null,
      imageUri: uriById.get(p.imageId) ?? "",
    });
  }
  return out;
}
```

Update `loadTask` signature + body:

```ts
async function loadTask(
  taskId: string,
  db: Db,
  sessionsClient: SessionsClient,
  profiles: ProfileStore,
  imagesClient: ImagesClient,
): Promise<Task> {
  // … fetch task row (unchanged) …
  const sessionRefRows = await db.select().from(taskSessionTable).where(eq(taskSessionTable.taskId, taskId));
  // … fetch sessionMap (unchanged) …
  const profileMap = await buildProfileMap(sessionRefRows, profiles, imagesClient);
  return buildTask(taskRow, sessionRefRows, sessionMap, profileMap);
}
```

Update the two `loadTask(...)` call sites in `createTask` and `getTask` to pass `profiles, imagesClient`.

Update `listTasks`: after fetching `sessionRefRows`, build the map once and pass it:

```ts
      const profileMap = await buildProfileMap(sessionRefRows, profiles, imagesClient);
      // … in the visibleTasks loop:
      visibleTasks.push(buildTask(row, refs, sessionMap, profileMap));
```

(`refs` in `listTasks` is grouped from `sessionRefRows`; ensure the grouped ref objects now also carry `profileId`: change `existing.push({ sessionId: ref.sessionId, role: ref.role })` → `existing.push({ sessionId: ref.sessionId, role: ref.role, profileId: ref.profileId })`, and update the `refsByTaskId` map's value type accordingly.)

`getTask` already calls `loadTask`; just pass the new args.

- [ ] **Step 2: Write the test** — add to `tasks.test.ts` a DB-gated assertion that the snapshot appears. In the CRUD `describe` (which now seeds a profile), extend the CreateTask assertion:

```ts
test.skipIf(!dbReachable)("CreateTask response carries the profile snapshot", async () => {
  const resp = await client.createTask({ type: "chat", profileId: PROFILE_ID, title: "snap" });
  const ref = resp.task!.sessions[0]!;
  expect(ref.profile).toBeDefined();
  expect(ref.profile!.id).toBe(PROFILE_ID);
  expect(ref.profile!.name).toBe("CRUD");
  expect(ref.profile!.archived).toBe(false);
  expect(ref.profile!.imageUri).toBe("registry/img-1:latest");
  createdTaskId = resp.task!.id;
});
```

(Replace the existing CreateTask test in that block, or add alongside and reuse `createdTaskId`.)

- [ ] **Step 3: Run**

Run: `cd orchestrator && ORCHESTRATOR_DATABASE_URL="$ORCHESTRATOR_DATABASE_URL" bun test src/__tests__/tasks.test.ts`
Expected: PASS.

- [ ] **Step 4: Typecheck the orchestrator**

Run: `cd orchestrator && bun run typecheck`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add orchestrator/src/rpc/tasks.ts orchestrator/src/__tests__/tasks.test.ts
git commit -m "$(cat <<'EOF'
feat(orchestrator): embed ProfileSnapshot on TaskSessionRef (ADR 0052)

ListTasks/GetTask/CreateTask resolve task_session.profile_id -> a
lightweight {id,name,icon,archived,image_uri} snapshot (one image-catalog
call + one profile query per response) for the app-wide profile chip.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 8: `DisableImage` profile guard (passthrough pre-flight)

**Files:**
- Modify: `orchestrator/src/rpc/passthrough.ts` (add optional `preflight` param)
- Create: `orchestrator/src/rpc/image-guard.ts`
- Modify: `orchestrator/src/index.ts` (wire the guard)
- Test: `orchestrator/src/__tests__/image-guard.test.ts`

- [ ] **Step 1: Add the pre-flight hook to the passthrough** — in `orchestrator/src/rpc/passthrough.ts`:

Add a type near the other exported types:

```ts
/** Optional per-method pre-flight, keyed by policyKey ("ImageService.DisableImage").
 *  Runs AFTER the authz gate and BEFORE the upstream forward. Throws to block. */
export type Preflight = (req: unknown, ctx: HandlerContext) => Promise<void>;
```

Extend `registerPassthrough` with a 6th positional param:

```ts
export function registerPassthrough(
  router: ConnectRouter,
  specs: PassthroughSpec[],
  upstream: Transport,
  getSession?: GetSession,
  resolveOwner?: ResolveOwner,
  preflight?: Record<string, Preflight>,
): void {
```

Inside the `for (const m of service.methods)` loop, compute the key once (it's currently computed inside `gate`):

```ts
      const key = policyKey(service.typeName, m);
```

Then change `gate` to use the outer `key` (remove its local `const key = …`), and in the **unary** handler run the pre-flight after the gate:

```ts
        impl[m.localName] = async (req: unknown, ctx: HandlerContext) => {
          await gate(req, ctx);
          const pf = preflight?.[key];
          if (pf) await pf(req, ctx);
          const res = await upstream.unary(/* …unchanged… */);
          // …unchanged…
        };
```

(The streaming branch needs no pre-flight; leave it. `DisableImage` is unary.)

- [ ] **Step 2: Write the guard** — create `orchestrator/src/rpc/image-guard.ts`:

```ts
/**
 * DisableImage profile guard (ADR 0052 §3).
 *
 * The coordinator 409s DisableImage when live SESSIONS reference an image, but
 * it knows nothing about orchestrator PROFILES. This pre-flight (run on the
 * passthrough before forwarding) blocks disabling an image that any ACTIVE
 * profile references, with failed_precondition + the blocking profile names.
 *
 * DisableImageRequest carries image_uri, while profiles store image_id — so we
 * resolve image_uri -> enabled_images.id via ListEnabledImages first. If the
 * uri isn't in the catalog (already disabled / unknown), we don't block and let
 * the upstream handle it (idempotent 204).
 */

import { ConnectError, Code } from "@connectrpc/connect";
import type { HandlerContext } from "@connectrpc/connect";

import { getDb } from "../db/client.ts";
import { makeProfileStore, type ProfileStore } from "../db/profiles.ts";
import { images as defaultImages } from "../control-plane/client.ts";
import type { ImagesClient } from "./tasks.ts";

export function makeDisableImageGuard(deps?: {
  store?: ProfileStore;
  images?: ImagesClient;
}): (req: unknown, ctx: HandlerContext) => Promise<void> {
  const store: ProfileStore = deps?.store ?? makeProfileStore(getDb());
  const images: ImagesClient = deps?.images ?? (defaultImages as unknown as ImagesClient);

  return async (req: unknown) => {
    const imageUri = (req as { imageUri?: string }).imageUri;
    if (!imageUri) return;

    const catalog = await images.listEnabledImages({});
    const match = catalog.images.find((i) => i.imageUri === imageUri);
    if (!match) return; // not enabled / unknown → let upstream be idempotent

    const active = (await store.list({ includeArchived: false })).filter((p) => p.imageId === match.id);
    if (active.length > 0) {
      const names = active.map((p) => p.name).join(", ");
      throw new ConnectError(
        `Can't disable — ${active.length} profile${active.length === 1 ? "" : "s"} use this image: ${names}`,
        Code.FailedPrecondition,
      );
    }
  };
}
```

- [ ] **Step 3: Wire it in `index.ts`** — in `orchestrator/src/index.ts`:

```ts
import { makeDisableImageGuard } from "./rpc/image-guard.ts";
```

Change the passthrough registration to pass the pre-flight (positional args: `getSession`/`resolveOwner` default to `undefined`):

```ts
    registerPassthrough(router, SURFACE, controlPlaneTransport, undefined, undefined, {
      "ImageService.DisableImage": makeDisableImageGuard(),
    });
```

- [ ] **Step 4: Write the test** — create `orchestrator/src/__tests__/image-guard.test.ts`:

```ts
import { expect, test, describe } from "bun:test";
import { ConnectError, Code } from "@connectrpc/connect";
import { makeDisableImageGuard } from "../rpc/image-guard.ts";
import type { ProfileStore, ProfileRow } from "../db/profiles.ts";
import type { ImagesClient } from "../rpc/tasks.ts";

const images: ImagesClient = {
  async listEnabledImages() {
    return { images: [{ id: "img-1", imageUri: "registry/api:latest" }] };
  },
};

function storeWith(profiles: Partial<ProfileRow>[]): ProfileStore {
  const rows = profiles.map((p, i) => ({
    id: `p${i}`, name: p.name ?? `P${i}`, description: "", icon: "Bot",
    imageId: p.imageId ?? "img-1", includeUserTokens: false, envVars: {},
    createdAt: new Date(0), updatedAt: new Date(0), deletedAt: p.deletedAt ?? null,
  })) as ProfileRow[];
  return {
    async list({ includeArchived }) { return rows.filter((r) => includeArchived || !r.deletedAt); },
    async get() { return null; }, async getActive() { return null; }, async getByIds() { return []; },
    async create() { throw new Error("unused"); }, async update() { return null; }, async softDelete() {},
  };
}

const ctx = {} as never;

describe("DisableImage guard", () => {
  test("blocks when an active profile references the image", async () => {
    const guard = makeDisableImageGuard({ images, store: storeWith([{ name: "Backend", imageId: "img-1" }]) });
    try {
      await guard({ imageUri: "registry/api:latest" }, ctx);
      throw new Error("expected to throw");
    } catch (e) {
      expect(e).toBeInstanceOf(ConnectError);
      expect((e as ConnectError).code).toBe(Code.FailedPrecondition);
      expect((e as ConnectError).message).toContain("Backend");
    }
  });

  test("allows when only an archived profile references it", async () => {
    const guard = makeDisableImageGuard({ images, store: storeWith([{ name: "Old", imageId: "img-1", deletedAt: new Date(0) }]) });
    await guard({ imageUri: "registry/api:latest" }, ctx); // no throw
  });

  test("allows when no profile references it", async () => {
    const guard = makeDisableImageGuard({ images, store: storeWith([{ name: "Other", imageId: "img-2" }]) });
    await guard({ imageUri: "registry/api:latest" }, ctx); // no throw
  });

  test("no-op for an unknown / already-disabled uri", async () => {
    const guard = makeDisableImageGuard({ images, store: storeWith([{ name: "Backend", imageId: "img-1" }]) });
    await guard({ imageUri: "registry/gone:latest" }, ctx); // not in catalog → no throw
  });
});
```

- [ ] **Step 5: Run + conformance**

Run: `cd orchestrator && bun test src/__tests__/image-guard.test.ts src/__tests__/passthrough.conformance.test.ts`
Expected: PASS (the conformance test still passes — `DisableImage` stays in `SURFACE`; only a pre-flight was added).

- [ ] **Step 6: Commit**

```bash
git add orchestrator/src/rpc/passthrough.ts orchestrator/src/rpc/image-guard.ts orchestrator/src/index.ts orchestrator/src/__tests__/image-guard.test.ts
git commit -m "$(cat <<'EOF'
feat(orchestrator): block DisableImage when a profile uses it (ADR 0052)

Generic pre-flight hook on the passthrough; the DisableImage guard
resolves image_uri -> enabled_images.id and rejects with
failed_precondition + the blocking profile names when any active profile
references the image. Stacks in front of the coordinator's session guard.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase 3 — Web data layer

### Task 9: Add shadcn components + `"Profile"` web CASL subject

**Files:**
- Create: `web/src/components/ui/popover.tsx`, `hover-card.tsx`, `switch.tsx` (via shadcn CLI)
- Modify: `web/src/lib/ability.ts`

- [ ] **Step 1: Add the shadcn components**

Run: `cd web && npx shadcn@latest add popover hover-card switch`
Expected: creates `web/src/components/ui/popover.tsx`, `hover-card.tsx`, `switch.tsx`. (`command` and `tooltip` already exist.) If the CLI prompts, accept defaults (new-york / lucide, matching `components.json`).

- [ ] **Step 2: Add the `"Profile"` subject** — in `web/src/lib/ability.ts`, mirror the orchestrator change:

```ts
export type Subjects = "Task" | "Session" | "EnabledImage" | "Profile" | "Fleet" | "Registry" | "all";
```

In `abilityFor`, after `can("read", "EnabledImage");`:

```ts
  // Profiles: every member reads the picker menu; mutations are admin-only
  // (manage("all")). ADR 0052 §6.
  can("read", "Profile");
```

- [ ] **Step 3: Typecheck**

Run: `cd web && npx tsc -b --noEmit`
Expected: clean (the new ui components compile; ability change is type-only).

- [ ] **Step 4: Commit**

```bash
git add web/src/components/ui/popover.tsx web/src/components/ui/hover-card.tsx web/src/components/ui/switch.tsx web/src/lib/ability.ts
git commit -m "$(cat <<'EOF'
feat(web): add popover/hover-card/switch + Profile CASL subject (ADR 0052)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 10: Profile query/mutation hooks

**Files:**
- Create: `web/src/hooks/useProfiles.ts`
- Test: none yet (covered via the picker/editor tests in later tasks); this task is wiring only.

- [ ] **Step 1: Write the hooks** — create `web/src/hooks/useProfiles.ts`:

```ts
import { useMutation, useQuery, createConnectQueryKey } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";
import {
  listProfiles,
  getProfile,
  createProfile,
  updateProfile,
  deleteProfile,
} from "../gen/engram/app/v1/profile-ProfileService_connectquery";

/** Active profiles (the picker menu). Admins can pass includeArchived. */
export function useProfiles(includeArchived = false) {
  return useQuery(listProfiles, { includeArchived }, { staleTime: 10_000 });
}

export function useProfile(id: string | undefined) {
  return useQuery(getProfile, { id: id ?? "" }, { enabled: !!id });
}

/** Invalidate every listProfiles variant (archived + active) after a mutation. */
function useInvalidateProfiles() {
  const qc = useQueryClient();
  return () =>
    qc.invalidateQueries({
      queryKey: createConnectQueryKey({ schema: listProfiles, cardinality: "finite" }),
    });
}

export function useCreateProfile() {
  const invalidate = useInvalidateProfiles();
  return useMutation(createProfile, { onSuccess: invalidate });
}

export function useUpdateProfile() {
  const invalidate = useInvalidateProfiles();
  return useMutation(updateProfile, { onSuccess: invalidate });
}

export function useDeleteProfile() {
  const invalidate = useInvalidateProfiles();
  return useMutation(deleteProfile, { onSuccess: invalidate });
}
```

- [ ] **Step 2: Typecheck**

Run: `cd web && npx tsc -b --noEmit`
Expected: clean (the generated `profile-ProfileService_connectquery` exists from Task 1).

- [ ] **Step 3: Commit**

```bash
git add web/src/hooks/useProfiles.ts
git commit -m "$(cat <<'EOF'
feat(web): profile query/mutation hooks (ADR 0052)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase 4 — New-session picker

### Task 11: Rewrite `NewSessionDialog` as a profile picker

**Files:**
- Modify: `web/src/components/NewSessionDialog.tsx`
- Create: `web/src/components/profiles/ProfileIcon.tsx` (a tiny name→lucide resolver, reused by the chip/editor)
- Test: `web/src/components/NewSessionDialog.test.tsx`

- [ ] **Step 1: Write the icon resolver** — create `web/src/components/profiles/ProfileIcon.tsx`:

```tsx
import * as Lucide from "lucide-react";
import { Box } from "lucide-react";
import type { LucideProps } from "lucide-react";

/** Resolve a stored lucide icon name to its component, falling back to Box for
 * unknown names. Profiles store the icon name as a string (ADR §1). */
export function ProfileIcon({ name, ...props }: { name: string } & LucideProps) {
  const Icon = (Lucide as unknown as Record<string, React.ComponentType<LucideProps>>)[name] ?? Box;
  return <Icon {...props} />;
}

/** Curated, dev-relevant starter set for the icon picker (ADR §7). */
export const PROFILE_ICON_CHOICES = [
  "Bot", "Terminal", "Bug", "Wrench", "FlaskConical", "GitBranch", "Rocket", "Cpu",
  "Code", "Database", "Server", "Shield", "Box", "Boxes", "Cog", "Hammer",
] as const;
```

- [ ] **Step 2: Write the failing test** — create `web/src/components/NewSessionDialog.test.tsx`. (Match the existing web test idiom — check another `*.test.tsx` for the render/provider wrapper; assume a `renderWithProviders` or a local QueryClientProvider + RouterProvider. The assertions below are the behavior contract.)

```tsx
import { describe, it, expect, vi } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { NewSessionDialog } from "./NewSessionDialog";

// Mock the profile hooks + createTask mutation. Adjust mock mechanism to match
// the repo's existing component-test pattern (e.g. vi.mock on the hook modules).
vi.mock("../hooks/useProfiles", () => ({
  useProfiles: () => ({
    data: { profiles: [
      { id: "p1", name: "Backend Agent", description: "Node API", icon: "Server", imageId: "i1", includeUserTokens: true, envVars: {}, archived: false },
      { id: "p2", name: "Frontend Agent", description: "React app", icon: "Code", imageId: "i2", includeUserTokens: false, envVars: {}, archived: false },
    ] },
    isPending: false,
  }),
}));

describe("NewSessionDialog (profile picker)", () => {
  it("lists profiles and filters by the search box", async () => {
    render(<NewSessionDialog open onOpenChange={() => {}} showTrigger={false} onCreated={() => {}} />);
    expect(screen.getByText("Backend Agent")).toBeInTheDocument();
    expect(screen.getByText("Frontend Agent")).toBeInTheDocument();
    fireEvent.change(screen.getByPlaceholderText("Search profiles…"), { target: { value: "front" } });
    await waitFor(() => expect(screen.queryByText("Backend Agent")).not.toBeInTheDocument());
    expect(screen.getByText("Frontend Agent")).toBeInTheDocument();
  });

  it("shows the task field always (every profile is an agent session)", () => {
    render(<NewSessionDialog open onOpenChange={() => {}} showTrigger={false} onCreated={() => {}} />);
    expect(screen.getByPlaceholderText("Describe the task for this session…")).toBeInTheDocument();
  });
});
```

(If the repo has no empty-`profiles` mock variant test, also add an empty-state test asserting "No profiles configured" renders.)

- [ ] **Step 3: Run — expect fail**

Run: `cd web && npx vitest run src/components/NewSessionDialog.test.tsx`
Expected: FAIL (the current dialog renders an image dropdown, not profiles).

- [ ] **Step 4: Rewrite the component** — replace `web/src/components/NewSessionDialog.tsx` with:

```tsx
import { type ComponentProps, useMemo, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useMutation, createConnectQueryKey } from "@connectrpc/connect-query";
import { useForm } from "react-hook-form";
import { createTask, listTasks } from "../gen/engram/app/v1/task-TaskService_connectquery";
import { useProfiles } from "../hooks/useProfiles";
import { ProfileIcon } from "./profiles/ProfileIcon";
import { Button } from "@/components/ui/button";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle, DialogTrigger,
} from "@/components/ui/dialog";
import { Field, FieldError, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";

export function NewSessionDialog({
  onCreated, variant, className, triggerTestId,
  open: openProp, onOpenChange, showTrigger = true,
}: {
  onCreated: (id: string) => void;
  variant?: ComponentProps<typeof Button>["variant"];
  className?: string;
  triggerTestId?: string;
  open?: boolean;
  onOpenChange?: (open: boolean) => void;
  showTrigger?: boolean;
}) {
  const [internalOpen, setInternalOpen] = useState(false);
  const open = openProp ?? internalOpen;
  const setOpen = onOpenChange ?? setInternalOpen;

  const { data, isPending } = useProfiles(false);
  const profiles = data?.profiles ?? [];
  const qc = useQueryClient();
  const createTaskMutation = useMutation(createTask);

  const [search, setSearch] = useState("");
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const form = useForm<{ prompt: string }>({ defaultValues: { prompt: "" } });

  const filtered = useMemo(() => {
    const q = search.trim().toLowerCase();
    if (!q) return profiles;
    return profiles.filter(
      (p) => p.name.toLowerCase().includes(q) || p.description.toLowerCase().includes(q),
    );
  }, [profiles, search]);

  const selected = profiles.find((p) => p.id === selectedId) ?? null;

  const onSubmit = async (values: { prompt: string }) => {
    if (!selected) return;
    try {
      const res = await createTaskMutation.mutateAsync({
        type: "chat",
        profileId: selected.id,
        prompt: values.prompt.trim() ? values.prompt.trim() : undefined,
      });
      qc.invalidateQueries({
        queryKey: createConnectQueryKey({ schema: listTasks, cardinality: "finite" }),
      });
      const sessionId = res.task?.sessions[0]?.sessionId;
      if (sessionId) {
        setOpen(false);
        form.reset();
        setSelectedId(null);
        setSearch("");
        onCreated(sessionId);
      } else {
        form.setError("root", { message: "Task created but no session id returned." });
      }
    } catch (e) {
      form.setError("root", { message: e instanceof Error ? e.message : String(e) });
    }
  };

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      {showTrigger && (
        <DialogTrigger asChild>
          <Button data-testid={triggerTestId} variant={variant} className={className}>
            New session
          </Button>
        </DialogTrigger>
      )}
      <DialogContent>
        <DialogHeader>
          <DialogTitle>New session</DialogTitle>
          <DialogDescription>Pick a profile, then say what to run.</DialogDescription>
        </DialogHeader>

        {isPending && <p className="text-sm text-muted-foreground">Loading profiles…</p>}

        {!isPending && profiles.length === 0 && (
          <p className="text-sm text-muted-foreground">
            No profiles configured — contact an admin to set one up.
          </p>
        )}

        {!isPending && profiles.length > 0 && (
          <form onSubmit={form.handleSubmit(onSubmit)}>
            <FieldGroup>
              <Input
                placeholder="Search profiles…"
                value={search}
                onChange={(e) => setSearch(e.target.value)}
                autoFocus
              />

              <div className="flex max-h-72 flex-col gap-2 overflow-y-auto" role="radiogroup" aria-label="Profiles">
                {filtered.length === 0 && (
                  <p className="px-1 py-4 text-center text-sm text-muted-foreground">No matches.</p>
                )}
                {filtered.map((p) => (
                  <button
                    type="button"
                    key={p.id}
                    role="radio"
                    aria-checked={selectedId === p.id}
                    data-testid={`profile-row-${p.id}`}
                    onClick={() => setSelectedId(p.id)}
                    className={cn(
                      "flex items-start gap-3 rounded-md border p-3 text-left transition-colors",
                      selectedId === p.id ? "border-primary bg-accent" : "border-border hover:bg-accent/50",
                    )}
                  >
                    <ProfileIcon name={p.icon} className="mt-0.5 size-5 shrink-0 text-muted-foreground" />
                    <span className="min-w-0">
                      <span className="block font-medium">{p.name}</span>
                      <span className="block truncate text-sm text-muted-foreground">{p.description}</span>
                      {p.includeUserTokens && (
                        <span className="mt-1 block text-xs text-muted-foreground">carries your token</span>
                      )}
                    </span>
                  </button>
                ))}
              </div>

              <Field>
                <FieldLabel htmlFor="prompt">Task</FieldLabel>
                <Textarea
                  id="prompt"
                  rows={2}
                  placeholder="Describe the task for this session…"
                  {...form.register("prompt")}
                />
              </Field>

              {form.formState.errors.root && <FieldError errors={[form.formState.errors.root]} />}
            </FieldGroup>

            <DialogFooter className="mt-4">
              <Button type="submit" data-testid="start-session" disabled={!selected || form.formState.isSubmitting}>
                {form.formState.isSubmitting ? "Starting…" : "Start session"}
              </Button>
            </DialogFooter>
          </form>
        )}
      </DialogContent>
    </Dialog>
  );
}
```

- [ ] **Step 5: Run — expect pass**

Run: `cd web && npx vitest run src/components/NewSessionDialog.test.tsx`
Expected: PASS.

- [ ] **Step 6: Typecheck**

Run: `cd web && npx tsc -b --noEmit`
Expected: clean. (The old `useEnabledImages`/`createSession`/`useAuth` imports are gone from this file; if anything else imported `newSessionSchema` from here, update it — grep `from "../components/NewSessionDialog"`.)

- [ ] **Step 7: Commit**

```bash
git add web/src/components/NewSessionDialog.tsx web/src/components/profiles/ProfileIcon.tsx web/src/components/NewSessionDialog.test.tsx
git commit -m "$(cat <<'EOF'
feat(web): new-session dialog becomes a searchable profile picker (ADR 0052)

Replaces the image/mode fields with a searchable profile list (radio-card
rows + lucide glyph + "carries your token" meta). Task field always shown;
empty state when no profiles. Sends {type:'chat', profileId, prompt}.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase 5 — Admin management surface

### Task 12: Routes + nav for `/settings/profiles`

**Files:**
- Modify: `web/src/router.tsx`
- Modify: `web/src/pages/settings/SettingsLayout.tsx`
- Create: `web/src/pages/settings/SessionProfiles.tsx` (stub list — fleshed out in Task 13)
- Create: `web/src/pages/settings/SessionProfileEditor.tsx` (stub — fleshed out in Task 14)

- [ ] **Step 1: Create stub pages** so the routes compile.

`web/src/pages/settings/SessionProfiles.tsx`:

```tsx
export function SessionProfiles() {
  return <div data-testid="session-profiles">Session Profiles</div>;
}
```

`web/src/pages/settings/SessionProfileEditor.tsx`:

```tsx
export function SessionProfileEditor({ mode }: { mode: "create" | "edit" }) {
  return <div data-testid="session-profile-editor">{mode}</div>;
}
```

- [ ] **Step 2: Add the routes** — in `web/src/router.tsx`, mirroring `membersRoute`. Add route defs (near the other settings routes):

```tsx
const profilesRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "profiles",
  beforeLoad: requireAdmin,
  component: SessionProfiles,
});
const profilesNewRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "profiles/new",
  beforeLoad: requireAdmin,
  component: () => <SessionProfileEditor mode="create" />,
});
const profileEditRoute = createRoute({
  getParentRoute: () => settingsLayoutRoute,
  path: "profiles/$id",
  beforeLoad: requireAdmin,
  component: () => <SessionProfileEditor mode="edit" />,
});
```

Add the imports:

```tsx
import { SessionProfiles } from "./pages/settings/SessionProfiles";
import { SessionProfileEditor } from "./pages/settings/SessionProfileEditor";
```

Add them to the settings children in `routeTree` — **`profiles/new` must precede `profiles/$id`** so the literal wins over the param:

```tsx
    settingsLayoutRoute.addChildren([
      settingsIndexRoute, profileRoute, tokensRoute, membersRoute,
      profilesRoute, profilesNewRoute, profileEditRoute,
    ]),
```

- [ ] **Step 3: Add the nav item** — in `web/src/pages/settings/SettingsLayout.tsx`:

Add `Layers` to the lucide import:

```tsx
import { KeyRound, Users, UserCircle, Layers } from "lucide-react";
```

Extend `ORG`:

```tsx
const ORG: NavItem[] = [
  { to: "/settings/members", label: "Members", icon: Users },
  { to: "/settings/profiles", label: "Session Profiles", icon: Layers },
];
```

- [ ] **Step 4: Typecheck + run web tests**

Run: `cd web && npx tsc -b --noEmit && npx vitest run`
Expected: clean + green (existing tests unaffected; route tree compiles).

- [ ] **Step 5: Commit**

```bash
git add web/src/router.tsx web/src/pages/settings/SettingsLayout.tsx web/src/pages/settings/SessionProfiles.tsx web/src/pages/settings/SessionProfileEditor.tsx
git commit -m "$(cat <<'EOF'
feat(web): admin routes + nav for Session Profiles (ADR 0052)

/settings/profiles (+ /new, /$id) behind requireAdmin; "Session Profiles"
nav item under the Org group, disambiguated from the account Profile page.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 13: Management list page (active + archived)

**Files:**
- Modify: `web/src/pages/settings/SessionProfiles.tsx`
- Test: `web/src/pages/settings/SessionProfiles.test.tsx`

- [ ] **Step 1: Write the failing test** — `web/src/pages/settings/SessionProfiles.test.tsx`:

```tsx
import { describe, it, expect, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { SessionProfiles } from "./SessionProfiles";

vi.mock("../../hooks/useProfiles", () => ({
  useProfiles: (includeArchived: boolean) => ({
    data: { profiles: includeArchived
      ? [{ id: "a1", name: "Old", description: "", icon: "Bot", imageId: "i", includeUserTokens: false, envVars: {}, archived: true }]
      : [{ id: "p1", name: "Backend", description: "Node API", icon: "Server", imageId: "i", includeUserTokens: true, envVars: {}, archived: false }] },
    isPending: false,
  }),
  useDeleteProfile: () => ({ mutateAsync: vi.fn(), isPending: false }),
}));

describe("SessionProfiles list", () => {
  it("renders active profiles with a create affordance", () => {
    render(<SessionProfiles />);
    expect(screen.getByText("Backend")).toBeInTheDocument();
    expect(screen.getByText("carries your token")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: /create profile/i })).toBeInTheDocument();
  });
});
```

- [ ] **Step 2: Run — expect fail**

Run: `cd web && npx vitest run src/pages/settings/SessionProfiles.test.tsx`
Expected: FAIL (stub renders only "Session Profiles").

- [ ] **Step 3: Implement the list** — replace `web/src/pages/settings/SessionProfiles.tsx`:

```tsx
import { useState } from "react";
import { Link } from "@tanstack/react-router";
import { toast } from "sonner";
import { Layers, Pencil, Archive } from "lucide-react";
import { useProfiles, useDeleteProfile } from "../../hooks/useProfiles";
import { ProfileIcon } from "../../components/profiles/ProfileIcon";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent,
  AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle, AlertDialogTrigger,
} from "@/components/ui/alert-dialog";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";

type ProfileRow = { id: string; name: string; description: string; icon: string; includeUserTokens: boolean; archived: boolean };

function Row({ p, onArchive }: { p: ProfileRow; onArchive?: (id: string) => void }) {
  return (
    <div className="flex items-center gap-3 rounded-md border p-3" data-testid={`profile-${p.id}`}>
      <ProfileIcon name={p.icon} className="size-5 shrink-0 text-muted-foreground" />
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="font-medium">{p.name}</span>
          {p.archived && <Badge variant="secondary">archived</Badge>}
        </div>
        <p className="truncate text-sm text-muted-foreground">{p.description}</p>
        {p.includeUserTokens && <p className="text-xs text-muted-foreground">carries your token</p>}
      </div>
      {!p.archived && (
        <div className="flex items-center gap-1">
          <Button asChild variant="ghost" size="sm">
            <Link to="/settings/profiles/$id" params={{ id: p.id }}>
              <Pencil className="size-4" /> Edit
            </Link>
          </Button>
          {onArchive && (
            <AlertDialog>
              <AlertDialogTrigger asChild>
                <Button variant="ghost" size="sm"><Archive className="size-4" /> Archive</Button>
              </AlertDialogTrigger>
              <AlertDialogContent>
                <AlertDialogHeader>
                  <AlertDialogTitle>Archive “{p.name}”?</AlertDialogTitle>
                  <AlertDialogDescription>
                    New sessions can no longer be started from it. Existing sessions and their
                    history are unaffected.
                  </AlertDialogDescription>
                </AlertDialogHeader>
                <AlertDialogFooter>
                  <AlertDialogCancel>Cancel</AlertDialogCancel>
                  <AlertDialogAction onClick={() => onArchive(p.id)}>Archive profile</AlertDialogAction>
                </AlertDialogFooter>
              </AlertDialogContent>
            </AlertDialog>
          )}
        </div>
      )}
    </div>
  );
}

export function SessionProfiles() {
  const { data, isPending } = useProfiles(false);
  const { data: allData } = useProfiles(true);
  const del = useDeleteProfile();
  const [showArchived, setShowArchived] = useState(false);

  const active = data?.profiles ?? [];
  const archived = (allData?.profiles ?? []).filter((p) => p.archived);

  const onArchive = async (id: string) => {
    try {
      await del.mutateAsync({ id });
      toast.success("Profile archived");
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "Failed to archive profile");
    }
  };

  return (
    <div className="mx-auto flex max-w-3xl flex-col gap-6">
      <header className="flex items-center justify-between">
        <div className="flex items-center gap-2">
          <Layers className="size-5" />
          <h1 className="text-lg font-semibold">Session Profiles</h1>
        </div>
        <Button asChild>
          <Link to="/settings/profiles/new">Create profile</Link>
        </Button>
      </header>

      {isPending && <p className="text-sm text-muted-foreground">Loading…</p>}
      {!isPending && active.length === 0 && (
        <div className="rounded-md border border-dashed p-8 text-center">
          <p className="text-sm text-muted-foreground">No session profiles yet</p>
          <Button asChild className="mt-3"><Link to="/settings/profiles/new">Create profile</Link></Button>
        </div>
      )}

      <div className="flex flex-col gap-2">
        {active.map((p) => <Row key={p.id} p={p} onArchive={onArchive} />)}
      </div>

      {archived.length > 0 && (
        <Collapsible open={showArchived} onOpenChange={setShowArchived}>
          <CollapsibleTrigger asChild>
            <Button variant="ghost" size="sm" className="self-start">
              {showArchived ? "Hide" : "Show"} archived ({archived.length})
            </Button>
          </CollapsibleTrigger>
          <CollapsibleContent className="mt-2 flex flex-col gap-2">
            {archived.map((p) => <Row key={p.id} p={p} />)}
          </CollapsibleContent>
        </Collapsible>
      )}
    </div>
  );
}
```

- [ ] **Step 4: Run — expect pass**

Run: `cd web && npx vitest run src/pages/settings/SessionProfiles.test.tsx`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add web/src/pages/settings/SessionProfiles.tsx web/src/pages/settings/SessionProfiles.test.tsx
git commit -m "$(cat <<'EOF'
feat(web): Session Profiles management list (ADR 0052)

Admin list of active profiles (glyph + name + description + token
indicator + edit/archive) with a collapsible Archived section. Archive
uses soft-delete via DeleteProfile; an AlertDialog confirms.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 14: Create/Edit editor sub-page

**Files:**
- Modify: `web/src/pages/settings/SessionProfileEditor.tsx`
- Create: `web/src/components/profiles/IconPicker.tsx`
- Create: `web/src/components/profiles/EnvVarsEditor.tsx`
- Test: `web/src/pages/settings/SessionProfileEditor.test.tsx`

- [ ] **Step 1: Write the icon picker** — `web/src/components/profiles/IconPicker.tsx`:

```tsx
import { useState } from "react";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Command, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from "@/components/ui/command";
import { Button } from "@/components/ui/button";
import { ProfileIcon, PROFILE_ICON_CHOICES } from "./ProfileIcon";

export function IconPicker({ value, onChange }: { value: string; onChange: (name: string) => void }) {
  const [open, setOpen] = useState(false);
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button type="button" variant="outline" className="w-full justify-start gap-2" data-testid="icon-picker">
          <ProfileIcon name={value} className="size-4" />
          <span className="text-muted-foreground">{value}</span>
        </Button>
      </PopoverTrigger>
      <PopoverContent className="p-0" align="start">
        <Command>
          <CommandInput placeholder="Search icons…" />
          <CommandList>
            <CommandEmpty>No icons found.</CommandEmpty>
            <CommandGroup>
              {PROFILE_ICON_CHOICES.map((name) => (
                <CommandItem key={name} value={name} onSelect={() => { onChange(name); setOpen(false); }}>
                  <ProfileIcon name={name} className="mr-2 size-4" />
                  {name}
                </CommandItem>
              ))}
            </CommandGroup>
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  );
}
```

- [ ] **Step 2: Write the env editor** — `web/src/components/profiles/EnvVarsEditor.tsx`:

```tsx
import { Plus, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { FieldDescription } from "@/components/ui/field";

export type EnvRow = { key: string; value: string };

export function envRowsToMap(rows: EnvRow[]): Record<string, string> {
  const out: Record<string, string> = {};
  for (const r of rows) if (r.key.trim()) out[r.key.trim()] = r.value;
  return out;
}
export function mapToEnvRows(map: Record<string, string>): EnvRow[] {
  return Object.entries(map).map(([key, value]) => ({ key, value }));
}

export function EnvVarsEditor({ rows, onChange }: { rows: EnvRow[]; onChange: (rows: EnvRow[]) => void }) {
  const set = (i: number, patch: Partial<EnvRow>) =>
    onChange(rows.map((r, idx) => (idx === i ? { ...r, ...patch } : r)));
  return (
    <div className="flex flex-col gap-2">
      {rows.map((r, i) => (
        <div key={i} className="flex items-center gap-2" data-testid="env-row">
          <Input
            className="font-mono" placeholder="KEY" value={r.key} spellCheck={false} autoCapitalize="off"
            onChange={(e) => set(i, { key: e.target.value })}
          />
          <Input
            className="font-mono" placeholder="value" value={r.value} spellCheck={false}
            onChange={(e) => set(i, { value: e.target.value })}
          />
          <Button type="button" variant="ghost" size="icon" aria-label="Remove" onClick={() => onChange(rows.filter((_, idx) => idx !== i))}>
            <X className="size-4" />
          </Button>
        </div>
      ))}
      <Button type="button" variant="outline" size="sm" className="self-start" onClick={() => onChange([...rows, { key: "", value: "" }])}>
        <Plus className="size-4" /> Add variable
      </Button>
      <FieldDescription>Set the model here, e.g. <code className="font-mono">ANTHROPIC_MODEL</code>. There is no separate model field.</FieldDescription>
    </div>
  );
}
```

- [ ] **Step 3: Write the failing test** — `web/src/pages/settings/SessionProfileEditor.test.tsx`:

```tsx
import { describe, it, expect, vi } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { SessionProfileEditor } from "./SessionProfileEditor";

const create = vi.fn().mockResolvedValue({ profile: { id: "new" } });
vi.mock("../../hooks/useProfiles", () => ({
  useProfile: () => ({ data: undefined, isPending: false }),
  useCreateProfile: () => ({ mutateAsync: create, isPending: false }),
  useUpdateProfile: () => ({ mutateAsync: vi.fn(), isPending: false }),
}));
vi.mock("../../hooks/useEnabledImages", () => ({
  useEnabledImages: () => ({ data: [{ id: "i1", image_uri: "registry/api:latest" }], isLoading: false }),
}));
vi.mock("@tanstack/react-router", async (orig) => ({ ...(await orig()), useNavigate: () => vi.fn() }));

describe("SessionProfileEditor (create)", () => {
  it("requires a name and image, then calls createProfile", async () => {
    render(<SessionProfileEditor mode="create" />);
    fireEvent.change(screen.getByLabelText(/name/i), { target: { value: "Backend Agent" } });
    fireEvent.click(screen.getByRole("button", { name: /create profile/i }));
    await waitFor(() => expect(create).toHaveBeenCalled());
    expect(create.mock.calls[0][0]).toMatchObject({ name: "Backend Agent", imageId: "i1" });
  });
});
```

- [ ] **Step 4: Run — expect fail**

Run: `cd web && npx vitest run src/pages/settings/SessionProfileEditor.test.tsx`
Expected: FAIL (stub).

- [ ] **Step 5: Implement the editor** — replace `web/src/pages/settings/SessionProfileEditor.tsx`:

```tsx
import { useEffect, useState } from "react";
import { useNavigate, useParams } from "@tanstack/react-router";
import { useForm, Controller } from "react-hook-form";
import { zodResolver } from "@hookform/resolvers/zod";
import * as z from "zod";
import { toast } from "sonner";
import { useProfile, useCreateProfile, useUpdateProfile } from "../../hooks/useProfiles";
import { useEnabledImages } from "../../hooks/useEnabledImages";
import { IconPicker } from "../../components/profiles/IconPicker";
import { EnvVarsEditor, envRowsToMap, mapToEnvRows, type EnvRow } from "../../components/profiles/EnvVarsEditor";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { Switch } from "@/components/ui/switch";
import {
  Field, FieldDescription, FieldError, FieldGroup, FieldLabel, FieldLegend, FieldSet,
} from "@/components/ui/field";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";

const schema = z.object({
  name: z.string().trim().min(1, "Name is required"),
  description: z.string(),
  icon: z.string().min(1),
  imageId: z.string().min(1, "Select an image"),
  includeUserTokens: z.boolean(),
});
type Values = z.infer<typeof schema>;

export function SessionProfileEditor({ mode }: { mode: "create" | "edit" }) {
  const navigate = useNavigate();
  const params = useParams({ strict: false }) as { id?: string };
  const editingId = mode === "edit" ? params.id : undefined;
  const { data: existing } = useProfile(editingId);
  const { data: images } = useEnabledImages(true);
  const create = useCreateProfile();
  const update = useUpdateProfile();
  const [envRows, setEnvRows] = useState<EnvRow[]>([]);

  const form = useForm<Values>({
    resolver: zodResolver(schema),
    defaultValues: { name: "", description: "", icon: "Bot", imageId: "", includeUserTokens: false },
  });

  // Hydrate when editing (existing arrives async).
  useEffect(() => {
    if (existing?.profile) {
      const p = existing.profile;
      form.reset({
        name: p.name, description: p.description, icon: p.icon,
        imageId: p.imageId, includeUserTokens: p.includeUserTokens,
      });
      setEnvRows(mapToEnvRows(p.envVars));
    }
  }, [existing, form]);

  const onSubmit = async (v: Values) => {
    const payload = { ...v, envVars: envRowsToMap(envRows) };
    try {
      if (mode === "edit" && editingId) {
        await update.mutateAsync({ id: editingId, ...payload });
        toast.success("Saved changes");
      } else {
        await create.mutateAsync(payload);
        toast.success("Profile created");
      }
      navigate({ to: "/settings/profiles" });
    } catch (e) {
      form.setError("root", { message: e instanceof Error ? e.message : String(e) });
    }
  };

  const busy = form.formState.isSubmitting || create.isPending || update.isPending;

  return (
    <form onSubmit={form.handleSubmit(onSubmit)} className="mx-auto flex max-w-2xl flex-col gap-6">
      <h1 className="text-lg font-semibold">{mode === "edit" ? "Edit profile" : "New profile"}</h1>

      <FieldSet>
        <FieldLegend>Identity</FieldLegend>
        <FieldGroup>
          <Controller name="name" control={form.control} render={({ field, fieldState }) => (
            <Field data-invalid={fieldState.invalid}>
              <FieldLabel htmlFor="name">Name</FieldLabel>
              <Input {...field} id="name" aria-invalid={fieldState.invalid} />
              {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
            </Field>
          )} />
          <Controller name="description" control={form.control} render={({ field }) => (
            <Field>
              <FieldLabel htmlFor="description">Description</FieldLabel>
              <Textarea {...field} id="description" rows={2} />
            </Field>
          )} />
          <Controller name="icon" control={form.control} render={({ field }) => (
            <Field>
              <FieldLabel>Icon</FieldLabel>
              <IconPicker value={field.value} onChange={field.onChange} />
            </Field>
          )} />
        </FieldGroup>
      </FieldSet>

      <FieldSet>
        <FieldLegend>Launch</FieldLegend>
        <FieldGroup>
          <Controller name="imageId" control={form.control} render={({ field, fieldState }) => (
            <Field data-invalid={fieldState.invalid}>
              <FieldLabel htmlFor="imageId">Image</FieldLabel>
              <Select value={field.value} onValueChange={field.onChange}>
                <SelectTrigger id="imageId" data-testid="image-select"><SelectValue placeholder="Select an image" /></SelectTrigger>
                <SelectContent>
                  {(images ?? []).map((i) => (
                    <SelectItem key={i.id} value={i.id}>{i.image_uri}</SelectItem>
                  ))}
                </SelectContent>
              </Select>
              {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
            </Field>
          )} />
        </FieldGroup>
      </FieldSet>

      <FieldSet>
        <FieldLegend>Environment</FieldLegend>
        <FieldGroup>
          <Controller name="includeUserTokens" control={form.control} render={({ field }) => (
            <Field orientation="horizontal">
              <Switch id="includeUserTokens" checked={field.value} onCheckedChange={field.onChange} />
              <div>
                <FieldLabel htmlFor="includeUserTokens">Include user tokens</FieldLabel>
                <FieldDescription>
                  Sessions started from this profile may carry the user's Claude credentials into the
                  sandbox. Leave off for untrusted or externally-facing images.
                </FieldDescription>
              </div>
            </Field>
          )} />
          <Field>
            <FieldLabel>Environment variables</FieldLabel>
            <EnvVarsEditor rows={envRows} onChange={setEnvRows} />
          </Field>
        </FieldGroup>
      </FieldSet>

      {form.formState.errors.root && <FieldError errors={[form.formState.errors.root]} />}

      <div className="flex gap-2">
        <Button type="submit" disabled={busy}>
          {mode === "edit" ? "Save changes" : "Create profile"}
        </Button>
        <Button type="button" variant="ghost" onClick={() => navigate({ to: "/settings/profiles" })} disabled={busy}>
          Cancel
        </Button>
      </div>
    </form>
  );
}
```

(If `Field` has no `orientation` prop in this repo's `field.tsx`, drop it and use a `flex items-center gap-3` wrapper instead — check the `field.tsx` API from Task setup.)

- [ ] **Step 6: Run — expect pass**

Run: `cd web && npx vitest run src/pages/settings/SessionProfileEditor.test.tsx`
Expected: PASS.

- [ ] **Step 7: Typecheck**

Run: `cd web && npx tsc -b --noEmit`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add web/src/pages/settings/SessionProfileEditor.tsx web/src/components/profiles/IconPicker.tsx web/src/components/profiles/EnvVarsEditor.tsx web/src/pages/settings/SessionProfileEditor.test.tsx
git commit -m "$(cat <<'EOF'
feat(web): Session Profile create/edit editor (ADR 0052)

Full sub-page form grouped Identity / Launch / Environment: lucide icon
picker (Popover + Command), image select (by id), include_user_tokens
switch with capability-grant copy, and a KEY/VALUE env editor (model set
here, no separate field).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase 6 — Profile chip + image-disable error surface

### Task 15: `<ProfileChip>` across list/rail/detail

**Files:**
- Modify: `web/src/lib/types.ts` (add `profile?` to `SessionListItem`)
- Modify: `web/src/hooks/useTasks.ts` (`taskToSessionListItem` maps `ref.profile`)
- Create: `web/src/components/profiles/ProfileChip.tsx`
- Modify: the sessions list row component, `SessionsRail.tsx`/`useRailSessions.ts`, and `SessionDetail` header to render `<ProfileChip>`
- Test: `web/src/components/profiles/ProfileChip.test.tsx`

- [ ] **Step 1: Extend `SessionListItem`** — in `web/src/lib/types.ts`, add an optional profile snapshot to the `SessionListItem` type:

```ts
export interface ProfileSnapshotView {
  id: string;
  name: string;
  icon: string;
  archived: boolean;
  imageUri: string;
}
// add to SessionListItem:
  profile?: ProfileSnapshotView | null;
```

- [ ] **Step 2: Map it in `taskToSessionListItem`** — in `web/src/hooks/useTasks.ts`, inside `taskToSessionListItem`, read the primary ref's snapshot:

```ts
  const ref = task.sessions[0];
  const sess = ref?.session;
  const snap = ref?.profile;
  // …in the returned object:
    profile: snap
      ? { id: snap.id, name: snap.name, icon: snap.icon, archived: snap.archived, imageUri: snap.imageUri }
      : null,
```

- [ ] **Step 3: Write the failing test** — `web/src/components/profiles/ProfileChip.test.tsx`:

```tsx
import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { ProfileChip } from "./ProfileChip";

describe("ProfileChip", () => {
  it("renders the profile name + glyph", () => {
    render(<ProfileChip profile={{ id: "p", name: "Backend", icon: "Server", archived: false, imageUri: "registry/api:latest" }} />);
    expect(screen.getByText("Backend")).toBeInTheDocument();
  });
  it("falls back to the image string when profile-less", () => {
    render(<ProfileChip profile={null} fallbackImage="registry/api:latest" />);
    expect(screen.getByText(/api:latest/)).toBeInTheDocument();
  });
  it("marks archived profiles", () => {
    render(<ProfileChip profile={{ id: "p", name: "Old", icon: "Bot", archived: true, imageUri: "x" }} />);
    expect(screen.getByText("Old")).toBeInTheDocument();
    expect(screen.getByText(/archived/i)).toBeInTheDocument();
  });
});
```

- [ ] **Step 4: Run — expect fail**

Run: `cd web && npx vitest run src/components/profiles/ProfileChip.test.tsx`
Expected: FAIL (no component).

- [ ] **Step 5: Implement `<ProfileChip>`** — `web/src/components/profiles/ProfileChip.tsx`:

```tsx
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { HoverCard, HoverCardContent, HoverCardTrigger } from "@/components/ui/hover-card";
import { Badge } from "@/components/ui/badge";
import { ProfileIcon } from "./ProfileIcon";
import { stripImageHost } from "@/lib/format"; // existing helper used by the list today
import type { ProfileSnapshotView } from "@/lib/types";

function Details({ p }: { p: ProfileSnapshotView }) {
  return (
    <div className="flex flex-col gap-1 text-sm">
      <div className="flex items-center gap-2 font-medium">
        <ProfileIcon name={p.icon} className="size-4" /> {p.name}
        {p.archived && <Badge variant="secondary">archived</Badge>}
      </div>
      <div className="font-mono text-xs text-muted-foreground">{p.imageUri}</div>
    </div>
  );
}

/**
 * App-wide profile identity chip (ADR §7). `disclosure="tooltip"` for dense link
 * rows (rail/list — the whole row is already a link); `disclosure="hovercard"`
 * for the detail header (opens on hover, focus, and click/tap). Falls back to the
 * image string for legacy / profile-less sessions.
 */
export function ProfileChip({
  profile, fallbackImage, disclosure = "tooltip", className,
}: {
  profile: ProfileSnapshotView | null | undefined;
  fallbackImage?: string;
  disclosure?: "tooltip" | "hovercard";
  className?: string;
}) {
  if (!profile) {
    return (
      <span className={`min-w-0 truncate font-mono text-xs text-muted-foreground ${className ?? ""}`}>
        {fallbackImage ? stripImageHost(fallbackImage) : "—"}
      </span>
    );
  }

  const label = (
    <span className={`inline-flex min-w-0 items-center gap-1.5 ${className ?? ""}`}>
      <ProfileIcon name={profile.icon} className="size-3.5 shrink-0 text-muted-foreground" />
      <span className="truncate">{profile.name}</span>
      {profile.archived && <Badge variant="secondary" className="px-1 py-0 text-[10px]">archived</Badge>}
    </span>
  );

  if (disclosure === "hovercard") {
    return (
      <HoverCard openDelay={100}>
        <HoverCardTrigger asChild><button type="button" className="text-left">{label}</button></HoverCardTrigger>
        <HoverCardContent align="start"><Details p={profile} /></HoverCardContent>
      </HoverCard>
    );
  }
  return (
    <Tooltip>
      <TooltipTrigger asChild>{label}</TooltipTrigger>
      <TooltipContent align="start"><Details p={profile} /></TooltipContent>
    </Tooltip>
  );
}
```

(Verify the `stripImageHost` import path — the Explore found it used in `sessions-list.tsx`; grep `stripImageHost` to confirm its module, and fix the import if it differs.)

- [ ] **Step 6: Wire into the list row + rail + detail.**

In the sessions list row (where `stripImageHost(s.image)` renders today — confirm file via `grep -rn stripImageHost web/src`), replace it with:

```tsx
<ProfileChip profile={s.profile} fallbackImage={s.image} className="text-xs" />
```

In `SessionsRail.tsx` rows, render `<ProfileChip profile={row.profile} fallbackImage={row.image} disclosure="tooltip" />` where the row currently shows the image (thread `profile` through `useRailSessions`'s `RailSessions` row shape from `SessionListItem.profile`).

In `SessionDetail` header, derive the snapshot from the warm `useTasks()` cache by session id and render the hovercard variant:

```tsx
import { useTasks } from "../../hooks/useTasks";
// inside the component, where `id` is the session id:
const { data: tasksData } = useTasks();
const profileSnap =
  tasksData?.tasks
    .flatMap((t) => t.sessions)
    .find((r) => r.sessionId === id)?.profile ?? null;
// in the header JSX:
<ProfileChip
  profile={profileSnap ? { id: profileSnap.id, name: profileSnap.name, icon: profileSnap.icon, archived: profileSnap.archived, imageUri: profileSnap.imageUri } : null}
  fallbackImage={session?.image}
  disclosure="hovercard"
/>
```

(`useTasks()` is already polled app-wide by the rail, so the cache is warm — this adds no new network round-trip beyond what's loaded. If `SessionDetail` doesn't currently import the rail/tasks, this is the one added `useTasks()` subscription.)

- [ ] **Step 7: Run + typecheck**

Run: `cd web && npx vitest run src/components/profiles/ProfileChip.test.tsx && npx tsc -b --noEmit`
Expected: PASS + clean.

- [ ] **Step 8: Commit**

```bash
git add web/src/lib/types.ts web/src/hooks/useTasks.ts web/src/components/profiles/ProfileChip.tsx web/src/components/profiles/ProfileChip.test.tsx web/src/pages/sessions
git commit -m "$(cat <<'EOF'
feat(web): app-wide ProfileChip on list/rail/detail (ADR 0052)

Renders the profile glyph + name (tooltip in dense link rows, hover-card
in the detail header) sourced from the TaskSessionRef snapshot; falls back
to the image string for legacy/profile-less sessions. Detail reads the
snapshot from the warm useTasks() cache (no passthrough enrichment).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 16: Surface the image-disable rejection in Operator → Images

**Files:**
- Modify: `web/src/components/settings/ImagesPanel.tsx` (the disable action's error handling)
- Test: extend the panel's test if one exists, else a focused render test

- [ ] **Step 1: Find the disable call site**

Run: `grep -rn "useDisableImage\|disable\b" web/src/components/settings/ImagesPanel.tsx`
Expected: locate where `useDisableImage().mutateAsync({ imageUri })` is called.

- [ ] **Step 2: Surface the message** — wrap the disable call so the `failed_precondition` ConnectError message (which now reads "Can't disable — N profiles use this image: …") is shown to the admin via a toast (and/or inline error), instead of being swallowed. Mirror the existing error idiom in the panel:

```tsx
import { toast } from "sonner";
// …
const disable = useDisableImage();
const onDisable = async (imageUri: string) => {
  try {
    await disable.mutateAsync({ imageUri });
    toast.success("Image disabled");
  } catch (e) {
    // ConnectError.message carries the orchestrator guard text (ADR §3) or the
    // coordinator's own image_in_use text — surface verbatim.
    toast.error(e instanceof Error ? e.message : "Failed to disable image");
  }
};
```

Wire `onDisable` to the existing disable button/confirm. Keep whatever inline-error pattern the panel already uses if it has one.

- [ ] **Step 3: Test**

If `ImagesPanel.test.tsx` exists, add a case: mock `useDisableImage` to reject with a `ConnectError(FailedPrecondition, "Can't disable — 1 profile uses this image: Backend")`, trigger the disable, and assert the message surfaces. Otherwise add a minimal `ImagesPanel.test.tsx` covering just that path.

Run: `cd web && npx vitest run src/components/settings/ImagesPanel.test.tsx`
Expected: PASS.

- [ ] **Step 4: Full web check**

Run: `cd web && npx vitest run && npx tsc -b --noEmit && npm run lint`
Expected: green + clean.

- [ ] **Step 5: Commit**

```bash
git add web/src/components/settings/ImagesPanel.tsx web/src/components/settings/ImagesPanel.test.tsx
git commit -m "$(cat <<'EOF'
feat(web): surface "image in use by profiles" on disable (ADR 0052)

The Operator → Images disable action now shows the orchestrator's
failed_precondition message (which profiles block the disable) instead
of swallowing it.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Final verification (after Task 16)

- [ ] **Orchestrator:** `cd orchestrator && bun run typecheck && ORCHESTRATOR_DATABASE_URL="$ORCHESTRATOR_DATABASE_URL" bun test` → all green.
- [ ] **Web:** `cd web && npx tsc -b --noEmit && npx vitest run && npm run lint` → all green.
- [ ] **Proto:** `buf lint crates/engram-protocol/proto && buf build crates/engram-protocol/proto` → clean. (`buf breaking` runs against `main` in CI; under the current WIRE policy, reserving `image_uri` + adding `profile_id` passes.)
- [ ] **Manual smoke (dev stack up):** create a profile in Settings → Session Profiles; open New session → it lists/searches the profile; start a session with a task; confirm the session boots and the chip shows the profile; try to disable that profile's image in Operator → Images and confirm the rejection message names the profile; archive the profile and confirm it moves to the Archived section and disappears from the picker.
- [ ] **Rollout note (ADR §Migration):** until at least one profile exists, the UI can't create sessions. Create the initial profiles as the first admin action after deploy.

---

## Self-Review

**Spec coverage (ADR 0052 → task):**
- §1 profile table (no mode/sort_order/model/created_by/is_active/starting_prompt) → Task 2 schema. ✓
- §2 task_session.profile_id intra-DB FK, nullable → Task 2. ✓
- §3 logical image_id + validate on create/update + resolve at session-create + restrict-on-disable guard → Tasks 5 (validate), 6 (resolve), 8 (guard). ✓
- §4 soft delete + admin archive → Tasks 3 (store), 5 (service), 13 (archived UI). ✓
- §5 CreateTask rewrite (load, resolve, mode=agent, token-gated harness_env precedence, profile_id) → Task 6. ✓
- §6 ProfileService (5 RPCs, field filtering, include_archived admin gate) + picker (search, empty/one-profile, no escape hatch) → Tasks 5, 11. ✓
- §7 deferrals → modeled-around, nothing built (no mode column, no sort_order, no team scope). ✓
- §9 snapshot on read responses → Task 7 (TaskSessionRef.profile) + Task 15 (chip). ✓
- Impeccable §5A/B/C, §6 states, §7 interactions, §8 content → Tasks 11 (picker + search + empty + task-always-shown), 13 (list + archived), 14 (editor groups, icon picker, env editor, token switch copy), 15 (chip tooltip/hovercard + fallback + archived). ✓
- Implications "new failure modes" → empty picker (11), image-disabled-from-under-a-profile (6, FailedPrecondition), cannot-disable-N-profiles (8 + 16). ✓

**Type consistency:** `ProfileStore`/`ProfileRow`/`ProfileInput` identical across `db/profiles.ts`, `rpc/profiles.ts`, `rpc/tasks.ts`, `rpc/image-guard.ts`. `ImagesClient` shared from `rpc/tasks.ts` (imported by profiles + image-guard). Proto field names (`profileId`, `includeUserTokens`, `envVars`, `imageId`, `imageUri`) consistent web↔orchestrator (connect-es camelCase). `ProfileSnapshotView` (web) mirrors proto `ProfileSnapshot`.

**Known cross-task coupling (called out inline):** Tasks 6 & 7 both edit `loadTask`/`buildTask` — apply Task 7's signature change during Task 6 so `tasks.ts` typechecks; commit them separately. Verify `field.tsx` `orientation` prop and `stripImageHost` import path against the tree before relying on them (notes inline).
