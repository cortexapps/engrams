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
  | "manage";

export type Subjects =
  | "Task"
  | "Session"
  | "EnabledImage"
  | "Profile"
  | "Fleet"
  | "Registry"
  | "all";

// Typed subject shapes — used with CASL's `subject()` helper.
export type TaskSubject = { createdByUserId: string | null };
export type SessionSubject = { createdByUserId: string | null };

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
  // admin-only (covered by manage("all") below). ADR 0052 §6.
  can("read", "Profile");

  // Admin override.
  if (user.role === "admin") can("manage", "all");

  return build();
}
