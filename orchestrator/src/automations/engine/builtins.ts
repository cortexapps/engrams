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
