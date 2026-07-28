/** Review-trigger ownership (ADR 0100 decision 12).
 *
 * Unknown triggers are treated as human: automation is the narrow exception,
 * and a new caller must never be silently deduplicated as a machine retry.
 */
export function isHumanReviewTrigger(trigger: string): boolean {
  return trigger !== "opened" && trigger !== "synchronize";
}
