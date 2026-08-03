/**
 * CASL ability factory (ADR 0051 Task 18).
 *
 * Defines the permission model for the orchestrator:
 *   - Every authenticated user may create a Task.
 *   - Users manage (read/update/delete) Tasks they own (createdByUserId).
 *   - Users may read/prompt/shell/delete Sessions they own.
 *   - Every authenticated user may read the enabled-image catalog.
 *   - admin role gets manage('all') — unrestricted.
 *
 * The `subject` helper (from @casl/ability) must be used when checking
 * ownership-scoped conditions so CASL can compare subject-fields correctly:
 *
 *   ability.can('prompt', subject('Session', { createdByUserId: uid }))
 */

import {
  AbilityBuilder,
  createMongoAbility,
  type MongoAbility,
} from "@casl/ability";

export type Actions =
  | "create"
  | "read"
  | "prompt"
  | "shell"
  | "delete"
  | "update"
  | "share"
  | "manage";

export type Subjects =
  | "Task"
  | "Session"
  | "EnabledImage"
  | "Profile"
  | "Harness"
  | "Review"
  | "Fleet"
  | "Registry"
  | "Artifact"
  | "all";

// Typed subject shapes — used with CASL's `subject()` helper.
export type TaskSubject = { createdByUserId: string | null };
export type SessionSubject = { createdByUserId: string | null };
export type ArtifactSubject = {
  ownerUserId: string | null;
  visibility: string;
};

// We use `MongoAbility<[Actions, any]>` rather than the fully-typed
// `MongoAbility<[Actions, Subjects]>` because CASL's `subject()` helper
// overloads fight the union-subject constraint at call sites when mixing
// plain string subjects ("Fleet") with shaped objects ({ createdByUserId }).
// If this file grows team/sharing semantics, that is the OpenFGA trigger —
// stop and write the ADR first.
// eslint-disable-next-line @typescript-eslint/no-explicit-any
export type AppAbility = MongoAbility<[Actions, any]>;

export interface AbilityUser {
  id: string;
  role: string; // 'admin' | 'user'
}

/**
 * Build a CASL AppAbility instance for the given user.
 */
export function abilityFor(user: AbilityUser): AppAbility {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const { can, build } = new AbilityBuilder<AppAbility>(createMongoAbility as any);

  // Any authenticated user can create a task.
  can("create", "Task");

  // Users manage tasks they own.
  can("manage", "Task", { createdByUserId: user.id });

  // Users can operate on sessions they own.
  can(["read", "prompt", "shell", "delete"], "Session", {
    createdByUserId: user.id,
  });

  // Image catalog is readable by anyone.
  can("read", "EnabledImage");

  // Profiles: the menu every member picks from is readable; mutations are
  // admin-only (covered by manage("all") below). ADR 0053 §6.
  can("read", "Profile");

  // Harness catalog (ADR 0063): the harness/model/effort selectors every member
  // sees are config (model ids, flags — never secrets), so the catalog is
  // readable; register/delete are admin-only (manage("all") below).
  can("read", "Harness");

  // PR reviews are an org-visible team dashboard (ADR 0100). Any member can
  // read them, and re-run one (create a fresh pass) the same way they can
  // trigger a review by command; enrollment config stays admin-only.
  can(["read", "create"], "Review");

  // Artifacts: owners hold every action on their own (read/update/
  // share/delete via manage); an artifact shared with the org
  // (visibility = "org") is readable by any member. The admin case
  // rides manage("all") below — no per-call branching.
  can("manage", "Artifact", { ownerUserId: user.id });
  can("read", "Artifact", { visibility: "org" });

  // Admin override.
  if (user.role === "admin") can("manage", "all");

  return build();
}
