import { timestampDate } from "@bufbuild/protobuf/wkt";

import type { Review } from "../../gen/engram/app/v1/review_pb";

/**
 * Review vocabulary — the same grammar the session glyphs use (shape carries the
 * meaning, colour is secondary, and a word always rides alongside), but for a
 * review pass's own lifecycle rather than a sandbox's.
 *
 *   ○  queued          ◐  finding      (the finder is reading)
 *   ◑  verifying       ✓  posted
 *   !  failed          ✕  halted
 *   ◌  superseded
 *
 * Tones come from the instrument palette, never the raw Tailwind ramp: signal
 * green for a finished pass, amber while work is in flight, instrument red for a
 * pass that needs attention, muted ink for the archival states. Lime
 * (`--primary`) is a fill and is never a status.
 */
export interface ReviewStage {
  label: string;
  glyph: string;
  /** CSS colour expression — an instrument token, not a palette class. */
  tone: string;
  /** True while the pass is doing work, so the glyph may breathe. */
  live: boolean;
}

const STAGES: Record<string, ReviewStage> = {
  queued: {
    label: "Queued",
    glyph: "○",
    tone: "var(--muted-foreground)",
    live: false,
  },
  finding: {
    label: "Finding",
    glyph: "◐",
    tone: "var(--instrument-caution)",
    live: true,
  },
  verifying: {
    label: "Verifying",
    glyph: "◑",
    tone: "var(--instrument-caution)",
    live: true,
  },
  posted: {
    label: "Posted",
    glyph: "✓",
    tone: "var(--instrument-nominal)",
    live: false,
  },
  failed: {
    label: "Failed",
    glyph: "!",
    tone: "var(--instrument-critical)",
    live: false,
  },
  halted: {
    label: "Halted",
    glyph: "✕",
    tone: "var(--muted-foreground)",
    live: false,
  },
  superseded: {
    label: "Superseded",
    glyph: "◌",
    tone: "var(--muted-foreground)",
    live: false,
  },
};

/** An unknown status still renders as itself rather than vanishing. */
export function stageOf(status: string): ReviewStage {
  return (
    STAGES[status] ?? {
      label: status,
      glyph: "○",
      tone: "var(--muted-foreground)",
      live: false,
    }
  );
}

const ACTIVE: ReadonlySet<string> = new Set(["queued", "finding", "verifying"]);

/** A pass still doing work — it will change on its own, so the UI polls. */
export function isActive(status: string): boolean {
  return ACTIVE.has(status);
}

/**
 * The session whose phase is running right now, so a live pass can offer a
 * "watch" affordance without being expanded. Empty outside an active phase, and
 * before the id has been stamped at kickoff.
 */
export function livePhaseSession(review: Review): string | undefined {
  if (review.status === "finding") return review.finderSessionId;
  if (review.status === "verifying") return review.verifierSessionId;
  return undefined;
}

/**
 * How a PR names itself. The title is captured at pass start and absent on every
 * review recorded before that landed (ADR 0100 d9, no backfill), so the number
 * is the honest fallback rather than a placeholder.
 */
export function prTitleOf(review: Review): string | undefined {
  return review.prTitle && review.prTitle.trim() !== "" ? review.prTitle : undefined;
}

/** `owner/name#881` — the coordinate, for when there is no title. */
export function prCoordinate(review: Review): string {
  return `${review.repo}#${review.prNumber}`;
}

export function prUrl(review: Review): string {
  return `https://github.com/${review.repo}/pull/${review.prNumber}`;
}

/** Deep link to the formal review engrams posted, when it posted one. */
export function postedReviewUrl(review: Review): string | undefined {
  if (!review.githubReviewId) return undefined;
  return `${prUrl(review)}#pullrequestreview-${review.githubReviewId}`;
}

/** When the pass started, or undefined when the record carries no timestamp. */
export function reviewCreatedAt(review: Review): Date | undefined {
  return review.createdAt ? timestampDate(review.createdAt) : undefined;
}

/** Short SHA for display; the full value belongs in a title attribute. */
export function shortSha(sha: string): string {
  return sha.slice(0, 7);
}

/** `+12 −4 across 2 files`, omitting whatever wasn't captured. */
export function diffSummary(review: Review): string | undefined {
  const parts: string[] = [];
  if (review.additions != null) parts.push(`+${review.additions}`);
  if (review.deletions != null) parts.push(`−${review.deletions}`);
  const size = parts.join(" ");
  if (review.changedFiles != null) {
    const files = `${review.changedFiles} ${review.changedFiles === 1 ? "file" : "files"}`;
    return size ? `${size} across ${files}` : files;
  }
  return size || undefined;
}

/** Severity, in the order a reader should meet it. */
export const SEVERITY_ORDER = ["critical", "high", "medium", "low"] as const;

export type Severity = (typeof SEVERITY_ORDER)[number];

/**
 * Severity tint. Critical and high are the instrument-critical hue at different
 * weights; medium is caution; low is deliberately quiet ink, because a low
 * finding competing visually with a critical one is a lie about the stakes.
 */
export function severityTone(severity: string): string {
  switch (severity) {
    case "critical":
    case "high":
      return "var(--instrument-critical)";
    case "medium":
      return "var(--instrument-caution)";
    default:
      return "var(--muted-foreground)";
  }
}

/** Per-severity counts as `[label, count]`, dropping the zeroes. */
export function severityCounts(review: Review): Array<[Severity, number]> {
  const counts = review.findingCounts;
  if (!counts) return [];
  return SEVERITY_ORDER.map(
    (severity) => [severity, counts[severity]] as [Severity, number],
  ).filter(([, count]) => count > 0);
}
