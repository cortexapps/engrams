/** Built-in automation registry seam (ADR 0119 D7).
 *
 * Phase 4 registers the PR-review and Slack-brain definitions here; the boot
 * hook seeds them with the seed-profile pattern. Empty in phase 1.
 */

import type { AutomationDefinition } from "./definition.ts";

export interface BuiltinAutomation {
  key: string;
  name: string;
  description: string;
  /** Bump on any graph or inputs-schema change; the seeder inserts a new
   * version when the stored content hash differs. */
  definitionVersion: number;
  definition: AutomationDefinition;
  /** Seed-time default input values (e.g. the enrollment lift). */
  defaultInputs(): Promise<Record<string, unknown>>;
}

const builtins = new Map<string, BuiltinAutomation>();

export function registerBuiltinAutomation(builtin: BuiltinAutomation): void {
  if (builtins.has(builtin.key)) {
    throw new Error(`builtin automation "${builtin.key}" registered twice`);
  }
  builtins.set(builtin.key, builtin);
}

export function listBuiltinAutomations(): BuiltinAutomation[] {
  return [...builtins.values()];
}

/** Three-way merge of a built-in's tunable config on a version bump
 * (ADR 0119 built-in editing model).
 *
 * For each tunable field: an org override that DIFFERS from the previously
 * shipped value is the org's deliberate choice and is kept; an override that
 * equals the old shipped value was never really edited, so the new shipped
 * value wins and the override is dropped (it would otherwise pin the org to a
 * stale default forever). Fields the org never touched are not overrides at
 * all and simply follow the new version. Structural changes never reach this
 * function: users cannot make them, so there is nothing to merge. */
export function mergeBuiltinOverrides(
  shippedOld: Record<string, unknown>,
  shippedNew: Record<string, unknown>,
  override: Record<string, unknown>,
): Record<string, unknown> {
  const kept: Record<string, unknown> = {};
  for (const [field, value] of Object.entries(override)) {
    const old = shippedOld[field];
    const next = shippedNew[field];
    if (deepEqual(value, old)) {
      // Equal to the old default — not an edit. Follow the new default.
      continue;
    }
    if (deepEqual(value, next)) {
      // The org already chose what we now ship: nothing left to override.
      continue;
    }
    kept[field] = value;
  }
  return kept;
}

function deepEqual(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (typeof a !== typeof b || a === null || b === null) return false;
  if (typeof a !== "object") return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  if (Array.isArray(a) && Array.isArray(b)) {
    return a.length === b.length && a.every((x, i) => deepEqual(x, b[i]));
  }
  const ka = Object.keys(a as object);
  const kb = Object.keys(b as object);
  if (ka.length !== kb.length) return false;
  return ka.every(
    (k) =>
      Object.prototype.hasOwnProperty.call(b, k) &&
      deepEqual((a as Record<string, unknown>)[k], (b as Record<string, unknown>)[k]),
  );
}
