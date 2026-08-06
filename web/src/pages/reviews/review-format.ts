import { timestampDate } from "@bufbuild/protobuf/wkt";
import {
  AtSign,
  CircleDot,
  GitMerge,
  GitPullRequest,
  GitPullRequestClosed,
  GitPullRequestDraft,
  RotateCcw,
  Terminal,
  Zap,
  type LucideIcon,
} from "lucide-react";

import type { Review, ReviewEvent, ReviewFinding } from "../../gen/engram/app/v1/review_pb";

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

/** A pass still doing work — it will change on its own, so the UI polls. */
export function isActive(review: Pick<Review, "active">): boolean {
  return review.active;
}

/**
 * What set a pass going.
 *
 * The stored value is the raw webhook action or dispatch kind — "synchronize",
 * "command" — which tells a reader nothing. So each one gets plain English plus
 * an icon answering the question actually being asked: did a machine start this,
 * or did a person?
 *
 *   ⚡  automation — the PR opened, or someone pushed to it
 *   @   a person asked for it in a PR comment
 *   ↺   a person pressed Re-run
 *   ▸   dispatched through the API
 */
export interface TriggerKind {
  label: string;
  icon: LucideIcon;
}

const TRIGGERS: Record<string, TriggerKind> = {
  opened: { label: "PR opened", icon: Zap },
  synchronize: { label: "New commits", icon: Zap },
  command: { label: "Mentioned", icon: AtSign },
  retry: { label: "Re-run", icon: RotateCcw },
  dispatch: { label: "Dispatched", icon: Terminal },
};

/** An unrecognised trigger keeps its raw name rather than being mislabelled as
 *  automation — a new one could just as easily be a new human path. */
export function triggerOf(trigger: string): TriggerKind {
  return TRIGGERS[trigger] ?? { label: trigger, icon: CircleDot };
}

/**
 * True when a PERSON asked for this pass. Automation is the ordinary case, so
 * only the exceptions are ever marked: a list where every row says "New commits"
 * is a column that costs width and carries no information.
 */
export function isHumanTrigger(review: Pick<Review, "humanTrigger">): boolean {
  return review.humanTrigger;
}

/**
 * How a PR names itself. Read from the PR's own record (ADR 0100 d11), so every
 * pass agrees and a pass that failed before it learned anything still shows the
 * real name. Absent only when no pass over this PR ever captured one, and the
 * number is then the honest fallback rather than a placeholder.
 */
export function prTitleOf(review: Review): string | undefined {
  return review.prTitle && review.prTitle.trim() !== "" ? review.prTitle : undefined;
}

/**
 * GitHub's own URL when we have it, and the coordinate otherwise.
 *
 * The stored URL is preferred because `repo` is a mutable name: after a rename
 * the reconstructed link only works while GitHub still redirects, and the stored
 * one is what GitHub itself last handed us.
 */
export function prUrl(review: Review): string {
  return review.prUrl || `https://github.com/${review.repo}/pull/${review.prNumber}`;
}

/** Deep link to the formal review engrams posted, when it posted one. */
export function postedReviewUrl(review: Review): string | undefined {
  if (!review.githubReviewId) return undefined;
  return `${prUrl(review)}#pullrequestreview-${review.githubReviewId}`;
}

/**
 * The code a finding is about, at the commit the pass actually read.
 *
 * Pinned to `headSha` rather than the branch: the branch has almost certainly
 * moved on, and a finding pointing at lines that have since shifted is worse than
 * no link at all. Deciding whether a finding is real means looking at the code, so
 * this is the one link the surface cannot do without.
 */
export function blobUrl(review: Review, finding: ReviewFinding): string {
  // Escape each segment but keep the separators: `src/foo#bar.ts` is a valid git
  // path, and unescaped it turns `#bar.ts` into the URL fragment — so the link
  // opens the wrong file AND loses the line anchor. `?` does the same.
  const path = finding.path.split("/").map(encodeURIComponent).join("/");
  const base = `https://github.com/${review.repo}/blob/${review.headSha}/${path}`;
  const start = finding.startLine;
  const end = finding.endLine;
  if (start && end && start !== end) return `${base}#L${start}-L${end}`;
  const line = end ?? start;
  return line ? `${base}#L${line}` : base;
}

/** The inline conversation a posted finding became, when it reached the PR. */
export function threadUrl(review: Review, finding: ReviewFinding): string | undefined {
  if (!finding.githubThreadId) return undefined;
  return `${prUrl(review)}#discussion_r${finding.githubThreadId}`;
}

/**
 * Why a pass ended badly, from the milestone that recorded the ending.
 *
 * Matched on the event's KIND, not on being last. Review-event writes are
 * best-effort and their failures are swallowed, so if the `failed` event never
 * landed the last entry could be `cloning: "finder"` — and presenting that as the
 * cause of death is worse than admitting we don't know.
 */
export function failureReason(review: Review, events: readonly ReviewEvent[]): string | undefined {
  if (review.status !== "failed" && review.status !== "halted") return undefined;
  // Manual reverse scan: `findLast` is above this project's lib target.
  for (let i = events.length - 1; i >= 0; i--) {
    const event = events[i];
    if (event && event.kind === review.status) return event.detail || undefined;
  }
  return undefined;
}

/** When the pass started, or undefined when the record carries no timestamp. */
export function reviewCreatedAt(review: Review): Date | undefined {
  return review.createdAt ? timestampDate(review.createdAt) : undefined;
}

/** Human span between two moments: "12s", "3m 4s", "1h 2m". */
export function shortDuration(ms: number): string {
  if (ms < 1000) return "<1s";
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) {
    const rem = s % 60;
    return rem ? `${m}m ${rem}s` : `${m}m`;
  }
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
}

/** How long a pass took, read from its own log entries — no wall clock is
 *  consulted, so a finished pass reports the same duration forever. */
export function passDuration(events: readonly ReviewEvent[]): string | undefined {
  const first = events[0]?.createdAt;
  const last = events[events.length - 1]?.createdAt;
  if (!first || !last) return undefined;
  const span = timestampDate(last).getTime() - timestampDate(first).getTime();
  return span > 0 ? shortDuration(span) : undefined;
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

/**
 * The four severities as four DISTINGUISHABLE tints, for the stacked bar on the
 * ledger row.
 *
 * `severityTone` cannot do this job: it paints critical and high with the same
 * token, which is honest beside a word ("2 high" says which one it is) and
 * useless inside a bar, where two adjacent segments of one hue read as a single
 * block. So the ramp interpolates the two instrument tokens rather than
 * introducing a colour: critical is the critical token, medium is caution, high
 * is the step between them, and low is quiet ink.
 *
 * The bar is redundant by construction — segments always run critical to low,
 * and the count beside it carries the numbers for anyone the hue fails.
 */
export const SEVERITY_BAR_TONE: Record<Severity, string> = {
  critical: "var(--instrument-critical)",
  high: "color-mix(in oklch, var(--instrument-critical) 58%, var(--instrument-caution))",
  medium: "var(--instrument-caution)",
  low: "color-mix(in oklch, var(--muted-foreground) 55%, transparent)",
};

/**
 * The pull request's own state, as the shape every developer already reads.
 *
 * Colour is deliberately not part of it. GitHub's purple-merge/green-open coding
 * is GitHub's palette, not this one, and the row already spends its colour on
 * the pass's stage — which is the row's actual subject. The shape carries the
 * state and the word goes to assistive tech.
 *
 * Undefined for a state that was never captured — older records predate the
 * field, and a guessed "Open" would be a claim we cannot make.
 */
export const PR_STATES: Record<string, { label: string; icon: LucideIcon } | undefined> = {
  open: { label: "Open", icon: GitPullRequest },
  draft: { label: "Draft", icon: GitPullRequestDraft },
  merged: { label: "Merged", icon: GitMerge },
  closed: { label: "Closed", icon: GitPullRequestClosed },
};

/** `+12 −4`, the size of the diff this pass read. Undefined when neither side
 *  was captured; a lone `+12` is still worth showing. */
export function diffShort(review: Review): string | undefined {
  const parts: string[] = [];
  if (review.additions != null) parts.push(`+${review.additions}`);
  if (review.deletions != null) parts.push(`−${review.deletions}`);
  return parts.length > 0 ? parts.join(" ") : undefined;
}

/** Per-severity counts as `[label, count]`, dropping the zeroes. */
export function severityCounts(review: Review): Array<[Severity, number]> {
  const counts = review.findingCounts;
  if (!counts) return [];
  return SEVERITY_ORDER.map(
    (severity) => [severity, counts[severity]] as [Severity, number],
  ).filter(([, count]) => count > 0);
}
