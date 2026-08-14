import type { SpecHumanPresenceReader, SpecPresence } from "../routes/spec-sync.ts";

let implementation: (SpecPresence & SpecHumanPresenceReader) | null = null;

/** Install the production presence service before tool execution starts. */
export function setSpecPresence(presence: SpecPresence & SpecHumanPresenceReader): void {
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

/** Stable seam for projections that include live human presence. */
export const specHumanPresence: SpecHumanPresenceReader = {
  async humanPresence(specId) {
    if (!implementation) throw new Error("The spec presence service is not configured");
    return implementation.humanPresence(specId);
  },
};
