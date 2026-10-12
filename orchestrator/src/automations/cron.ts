import { Cron } from "croner";

import { cronEntrypointOf, DefinitionError, type AutomationDefinition } from "./engine/definition.ts";

/** The automation's `next_fire_at`: the first occurrence after `after` of its
 * single cron entrypoint (the main trigger or an extra entrypoint), or null
 * when it has no cron trigger. Every write that can change the trigger or
 * the enabled flag goes through this; a cron automation left with a null
 * `next_fire_at` is never due and never fires.
 *
 * Throws DefinitionError on an invalid schedule or timezone. */
export function nextFireAtOf(definition: AutomationDefinition, after: Date): Date | null {
  const trigger = cronEntrypointOf(definition)?.trigger;
  if (trigger?.kind !== "cron") return null;
  try {
    new Intl.DateTimeFormat("en-US", { timeZone: trigger.timezone }).format(new Date(0));
  } catch {
    throw new DefinitionError(null, "trigger.timezone", `invalid IANA timezone "${trigger.timezone}"`);
  }
  let next: Date | null;
  try {
    next = new Cron(trigger.schedule, { timezone: trigger.timezone, paused: true }).nextRun(after);
  } catch (error) {
    throw new DefinitionError(
      null,
      "trigger.schedule",
      `invalid cron schedule: ${error instanceof Error ? error.message : String(error)}`,
    );
  }
  if (!next) throw new DefinitionError(null, "trigger.schedule", "cron schedule has no future occurrence");
  return next;
}
