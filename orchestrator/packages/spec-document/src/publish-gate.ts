/**
 * The publish gate condition (ADR 0114 D10, R34-R35).
 *
 * The gate is pure: it takes the section states, the open questions and the
 * gap-check freshness, and it returns what stops a publish. Both tiers read
 * the same function, so the browser never computes a readiness rule of its
 * own and then disagrees with the server.
 *
 * Two rules carry the whole gate:
 *
 * - A required section must be `confirmed`, or `n/a` with a reason (R34).
 *   Every other state is a blocker, and each blocker names its section so the
 *   dialog can put the person one click from it.
 * - An open question never blocks (R35). It needs an explicit acknowledgment
 *   instead, because a person who publishes with unanswered questions must see
 *   the count and read the questions in full.
 */

export type PublishGateSectionState = "empty" | "drafted" | "confirmed" | "n/a";

/** One section, as the gate reads it. */
export interface PublishGateSection {
  /** The stable section node id. A blocker anchors to it. */
  id: string;
  title: string;
  layerKey: string;
  required: boolean;
  state: PublishGateSectionState;
  naReason: string | null;
}

/** One open question that a publish carries into the tickets. */
export interface PublishGateQuestion {
  id: string;
  sectionId: string;
  sectionTitle: string;
  text: string;
}

/** Why one required section is not settled. */
export type PublishBlockerReason = "empty" | "drafted" | "na_without_reason";

export interface PublishBlocker {
  sectionId: string;
  sectionTitle: string;
  layerKey: string;
  state: PublishGateSectionState;
  reason: PublishBlockerReason;
}

export interface PublishGateInput {
  sections: readonly PublishGateSection[];
  openQuestions: readonly PublishGateQuestion[];
  /** True when no gap check has run for the current document revision (R30). */
  gapCheckStale: boolean;
}

export interface PublishGate {
  /** True when a publish may proceed, with an acknowledgment if one is due. */
  ready: boolean;
  blockers: readonly PublishBlocker[];
  /** Settled required sections over all required sections ("7 of 9 ready"). */
  settledRequiredCount: number;
  requiredCount: number;
  openQuestions: readonly PublishGateQuestion[];
  /** True when the person must acknowledge the open questions first (R35). */
  acknowledgmentRequired: boolean;
  /** True when the gate must run the gap check before it publishes (R30). */
  gapCheckRunRequired: boolean;
}

/** True when this section state settles a required section (R34). */
export function sectionIsSettled(section: PublishGateSection): boolean {
  if (section.state === "confirmed") return true;
  return section.state === "n/a" && (section.naReason ?? "").trim().length > 0;
}

/**
 * Evaluate the gate. The result is the same value the button state, the
 * blocked dialog and the server-side refusal all read.
 */
export function evaluatePublishGate(input: PublishGateInput): PublishGate {
  const required = input.sections.filter((section) => section.required);
  const blockers: PublishBlocker[] = [];
  for (const section of required) {
    if (sectionIsSettled(section)) continue;
    blockers.push({
      sectionId: section.id,
      sectionTitle: section.title,
      layerKey: section.layerKey,
      state: section.state,
      reason: blockerReason(section),
    });
  }
  return {
    ready: blockers.length === 0,
    blockers,
    settledRequiredCount: required.length - blockers.length,
    requiredCount: required.length,
    openQuestions: [...input.openQuestions],
    // An acknowledgment is due only when the gate would otherwise pass. A
    // blocked dialog must show the blockers, not a checkbox.
    acknowledgmentRequired: blockers.length === 0 && input.openQuestions.length > 0,
    gapCheckRunRequired: blockers.length === 0 && input.gapCheckStale,
  };
}

function blockerReason(section: PublishGateSection): PublishBlockerReason {
  if (section.state === "n/a") return "na_without_reason";
  return section.state === "drafted" ? "drafted" : "empty";
}
