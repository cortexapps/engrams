/**
 * A pull request as its forge last described it.
 *
 * The fields split across two tables downstream, and the split is not cosmetic:
 *
 *   IDENTITY (`review_target`, one row per PR, refreshed on every capture) —
 *   `providerId`, `title`, `author`, `state`, `url`. These describe the pull
 *   request itself, so the freshest capture is the truth.
 *
 *   PER-PASS (`review`, one row per pass, never rewritten) — `headBranch`,
 *   `baseBranch`, `additions`, `deletions`, `changedFiles`. These describe the
 *   code THIS pass read: the diff grows with every push and a PR can be
 *   retargeted, so overwriting them would make an old pass misreport what it
 *   actually reviewed.
 */
export interface PrContext {
  /** The forge's stable id — the durable identity, and the only field a rename
   *  or a transfer cannot move. `repo` is a mutable, reusable name. */
  providerId: string | null;
  title: string | null;
  /** Login of the PR author. */
  author: string | null;
  /** open | draft | closed | merged, as of this capture. */
  state: string | null;
  /** The forge's own URL, so a link never has to be rebuilt from a repo name
   *  that may since have changed. */
  url: string | null;
  /** The forge's own last-modified time. Deliveries are not ordered, so this is
   *  what lets a late one be recognised as stale. */
  providerUpdatedAt: Date | null;
  headBranch: string | null;
  baseBranch: string | null;
  additions: number | null;
  deletions: number | null;
  changedFiles: number | null;
}

/**
 * True when a capture carries every field we persist, so the API fetch adds
 * nothing and can be skipped.
 *
 * The list below is exhaustive over `PrContext` on purpose — this is not "enough
 * to proceed", it is "nothing left to learn". Checked against the DATA rather
 * than against a list of webhook actions we believe are complete: a wrong belief
 * about an action would be invisible, because it would silently persist nulls
 * instead of raising anything. Being wrong here costs one API call.
 */
export function isCompletePrContext(pr: PrContext): boolean {
  return (
    pr.providerId !== null &&
    pr.title !== null &&
    pr.author !== null &&
    pr.state !== null &&
    pr.url !== null &&
    pr.providerUpdatedAt !== null &&
    pr.headBranch !== null &&
    pr.baseBranch !== null &&
    pr.additions !== null &&
    pr.deletions !== null &&
    pr.changedFiles !== null
  );
}
