/**
 * CASL ability factory (ADR 0039 Task 18).
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
  | "Fleet"
  | "Registry"
  | "all";

// Typed subject shapes — used with CASL's `subject()` helper.
export type TaskSubject = { createdByUserId: string | null };
export type SessionSubject = { createdByUserId: string | null };

// We use the generic MongoAbility without subject-shape constraints so that
// we can use plain strings as subjects AND call `subject(name, obj)` for
// ownership checks. The conditions are validated at runtime by CASL's
// MongoDB matcher.
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

  // Admin override.
  if (user.role === "admin") can("manage", "all");

  return build();
}
