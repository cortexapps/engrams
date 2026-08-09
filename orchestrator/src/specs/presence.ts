import type { SpecPresence } from "../routes/spec-sync.ts";

let implementation: SpecPresence | null = null;

/** Install the production presence service before tool execution starts. */
export function setSpecPresence(presence: SpecPresence): void {
  implementation = presence;
}

/** Stable seam for spec tools that show section-level agent activity. */
export const specPresence: SpecPresence = {
  async enter(input) {
    if (!implementation) throw new Error("The spec presence service is not configured");
    await implementation.enter(input);
  },
  async leave(input) {
    if (!implementation) throw new Error("The spec presence service is not configured");
    await implementation.leave(input);
  },
};
