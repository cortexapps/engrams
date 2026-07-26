import { timestampDate } from "@bufbuild/protobuf/wkt";

import type { Review } from "../../gen/engram/app/v1/review_pb";

/**
 * A PR is the record; a pass is an entry in it (ADR 0100).
 *
 * A retry and every `synchronize` push mint a new `review` row, so the raw list
 * repeats the same PR — and buries which pass is current. Everything the ledger
 * and the rail show is grouped through here first.
 */
export interface PrGroup {
  /** Stable identity for the PR itself: "owner/name#881". */
  key: string;
  repo: string;
  prNumber: number;
  /** Every pass over this PR, newest first. Always at least one. */
  passes: Review[];
  /** The pass that speaks for this PR right now — the newest one. */
  latest: Review;
}

/** Milliseconds for ordering. Reviews with no timestamp sort oldest. */
function createdAtMs(review: Review): number {
  return review.createdAt ? timestampDate(review.createdAt).getTime() : 0;
}

/**
 * Group reviews by their PR, newest pass first within each group and newest
 * group first overall. Ordering is computed here rather than trusted from the
 * server so the rail, the ledger, and the dossier's pass history agree.
 */
export function groupByPr(reviews: readonly Review[]): PrGroup[] {
  const groups = new Map<string, Review[]>();
  for (const review of reviews) {
    const key = `${review.repo}#${review.prNumber}`;
    const existing = groups.get(key);
    if (existing) existing.push(review);
    else groups.set(key, [review]);
  }

  const out: PrGroup[] = [];
  for (const [key, passes] of groups) {
    passes.sort((a, b) => createdAtMs(b) - createdAtMs(a));
    // Non-null: a key only exists because at least one review produced it.
    const latest = passes[0]!;
    out.push({
      key,
      repo: latest.repo,
      prNumber: latest.prNumber,
      passes,
      latest,
    });
  }
  out.sort((a, b) => createdAtMs(b.latest) - createdAtMs(a.latest));
  return out;
}

/** The group a given pass belongs to, or undefined when it isn't in the list. */
export function groupOf(groups: readonly PrGroup[], reviewId: string): PrGroup | undefined {
  return groups.find((g) => g.passes.some((p) => p.id === reviewId));
}

/**
 * The pass that superseded this one, if any — the next-newer pass over the same
 * PR. Derived from the grouping rather than a stored flag, which is why the
 * `superseded` status is never written (ADR 0100).
 */
export function supersededBy(group: PrGroup, reviewId: string): Review | undefined {
  const index = group.passes.findIndex((p) => p.id === reviewId);
  if (index <= 0) return undefined;
  return group.passes[index - 1];
}
