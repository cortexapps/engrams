import { useState } from "react";
import {
  ChevronDown,
  ChevronRight,
  Code2,
  MessageSquare,
  ShieldCheck,
  ShieldX,
} from "lucide-react";

import type { Review, ReviewFinding, ReviewVerdict } from "../../gen/engram/app/v1/review_pb";
import { Markdown } from "../../components/Markdown";
import { Text } from "@/components/ui/text";
import { Skeleton } from "@/components/ui/skeleton";
import { groupByOutcome, type JudgedFinding, type Outcome } from "./review-findings";
import { blobUrl, isActive, severityTone, threadUrl } from "./review-format";

/**
 * The findings, grouped by what actually happened to them.
 *
 * Severity answers "how bad"; outcome answers the question a reader asks first —
 * did this reach the PR, and if not, why not. `ui_only` is one stored state
 * meaning four different things, so the reason is derived and named here rather
 * than collapsed into "shown here only" (ADR 0100).
 *
 * The group IS the verification status, which is why no card repeats it as a
 * badge: everything under "Posted to the pull request" was confirmed, everything
 * under "Refuted by the verifier" was not.
 */
const GROUP_COPY: Record<Outcome, { heading: string; note?: string }> = {
  posted: {
    heading: "Posted to the pull request",
  },
  unresolved: {
    heading: "No decision yet",
    note: "Confirmed and anchored, but this pass never reached the posting gate — so nothing has been decided about them either way.",
  },
  unverified: {
    heading: "Not verified",
    // Tense-neutral on purpose: on a live pass "never judged" would be a lie
    // about a verifier that simply hasn't got there yet.
    note: "An unverified finding never posts — the verifier’s silence counts as low confidence, not as agreement.",
  },
  no_anchor: {
    heading: "No line to anchor to",
    note: "Confirmed, but about a file rather than a line — nowhere to hang an inline comment.",
  },
  over_cap: {
    heading: "Over the comment cap",
    note: "Confirmed and anchored, but a pass posts at most ten inline comments.",
  },
  refuted: {
    heading: "Refuted by the verifier",
    note: "The verifier traced these and couldn’t reproduce the failure.",
  },
};

/**
 * Nothing to show is three different situations, and calling them all "nothing
 * reported yet" hides which one you are in.
 */
function emptyLine(review: Review): string {
  if (isActive(review)) return "Nothing reported yet.";
  if (review.status === "posted") return "The finder reported nothing on this change.";
  // A deliberate stop is not a crash, and saying "ended before it reported"
  // for both would read every halt as a failure.
  if (review.status === "halted") return "This pass was halted before it reported anything.";
  return "This pass failed before it reported anything.";
}

/** How many bodies a reader will take in without choosing one. Applied to the
 *  leading group's first N cards regardless of how big that group is, so the
 *  boundary is visible in the result: three open, the rest a list. */
const AUTO_OPEN_LIMIT = 3;

export function FindingsLedger({
  review,
  judged,
  loading,
}: {
  review: Review;
  judged: JudgedFinding[];
  loading: boolean;
}) {
  if (loading && judged.length === 0) return <Skeleton className="h-20 w-full" />;

  // No heading and no dashed box for an empty state: an outlined placeholder gives
  // absence the visual weight of content, which on a failed pass makes the
  // loudest thing on the page the thing saying the least.
  if (judged.length === 0) {
    return <p className="text-sm text-muted-foreground">{emptyLine(review)}</p>;
  }

  const groups = groupByOutcome(judged);

  return (
    <section className="flex flex-col gap-6">
      {groups.map((group, i) => (
        <OutcomeSection
          key={group.outcome}
          review={review}
          outcome={group.outcome}
          items={group.items}
          total={judged.length}
          // The leading group is what the pass actually produced, so its first
          // few open on arrival: one posted finding needs no clicks, and forty
          // stay a list you can scan.
          autoOpen={i === 0 ? AUTO_OPEN_LIMIT : 0}
        />
      ))}
    </section>
  );
}

/** Refuted findings start collapsed as a group: they are the verifier's working,
 *  not the pass's output, and the count is the part that matters at a glance. */
function OutcomeSection({
  review,
  outcome,
  items,
  total,
  autoOpen,
}: {
  review: Review;
  outcome: Outcome;
  items: JudgedFinding[];
  total: number;
  /** How many of this group's cards arrive open. */
  autoOpen: number;
}) {
  const collapsible = outcome === "refuted";
  const [open, setOpen] = useState(!collapsible);
  const copy = GROUP_COPY[outcome];
  const Chevron = open ? ChevronDown : ChevronRight;

  // Sentence case at body weight, not a tracked uppercase label: five outcome
  // groups in eyebrow caps turns the findings into a stack of section banners
  // with the actual findings hiding between them. It is still an h2 — the group IS
  // the reason a finding didn't post, so it has to be reachable by heading.
  const heading = (
    <>
      <Text as="h2" variant="body" className="text-sm font-medium">
        {copy.heading}
      </Text>
      <span className="ml-auto shrink-0 font-mono text-xs tabular-nums text-muted-foreground">
        {collapsible ? `${items.length} of ${total}` : items.length}
      </span>
    </>
  );

  return (
    <div className="flex flex-col gap-2">
      {/* Sticky, because the heading is the only place the outcome is stated: a
          card scrolled away from it can no longer say why it didn't reach the PR. */}
      <div className="sticky top-0 z-10 -mx-1 bg-background/95 px-1 py-1 backdrop-blur-sm">
        {collapsible ? (
          <button
            type="button"
            onClick={() => setOpen((v) => !v)}
            aria-expanded={open}
            className="flex w-full items-baseline gap-1.5 rounded-md text-left outline-none transition-colors hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring"
          >
            <Chevron className="size-3.5 shrink-0 text-muted-foreground" aria-hidden />
            {heading}
          </button>
        ) : (
          <div className="flex items-baseline gap-1.5">{heading}</div>
        )}
      </div>

      {open && (
        <>
          {copy.note && <p className="text-xs text-muted-foreground">{copy.note}</p>}
          <ul className="flex flex-col gap-2">
            {items.map((item, i) => (
              <li key={item.finding.id}>
                <FindingCard
                  review={review}
                  finding={item.finding}
                  verdict={item.verdict}
                  defaultOpen={i < autoOpen}
                />
              </li>
            ))}
          </ul>
        </>
      )}
    </div>
  );
}

/** `path:L42-L45`, or just the path when the finding is about a whole file. */
function anchorOf(finding: ReviewFinding): string {
  const { startLine: start, endLine: end } = finding;
  if (start && end && start !== end) return `${finding.path}:L${start}-L${end}`;
  const line = end ?? start;
  return line ? `${finding.path}:L${line}` : finding.path;
}

const CATEGORY_LABELS: Record<string, string> = {
  "security-privacy": "Security & privacy",
  "stability-availability": "Stability & availability",
  "data-integrity-integration": "Data integrity",
  "functional-correctness": "Correctness",
  "performance-scalability": "Performance",
  "maintainability-quality": "Maintainability",
};

/**
 * One finding, collapsed to what a reader scans by and opening to what they read.
 *
 * A body runs to twenty lines of prose and the cap is 200 findings, so rendering
 * every body is the wall of text this page used to be. Collapsed, the severity
 * aligns into a column and the titles form a list; open, the finder's claim is the
 * body and the verifier's ruling is a separate attributed block below it — never
 * blended, because reading them as one paragraph is what made the old panel
 * untrustworthy. You couldn't tell which of them was making a given assertion.
 */
function FindingCard({
  review,
  finding,
  verdict,
  defaultOpen,
}: {
  review: Review;
  finding: ReviewFinding;
  verdict: ReviewVerdict | undefined;
  defaultOpen: boolean;
}) {
  const [open, setOpen] = useState(defaultOpen);
  const [showEvidence, setShowEvidence] = useState(false);
  const Chevron = open ? ChevronDown : ChevronRight;
  const thread = threadUrl(review, finding);

  return (
    <article className="overflow-hidden rounded-lg border bg-card">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        className="flex w-full items-start gap-2.5 px-3 py-2.5 text-left outline-none transition-colors hover:bg-accent/40 focus-visible:bg-accent/40 focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-inset"
      >
        <Chevron className="mt-0.5 size-3.5 shrink-0 text-muted-foreground" aria-hidden />
        {/* A dot in the severity's tone, then the word in ink, in a fixed column.
            No pill, so a low finding can't shout as loudly as a critical one; and
            the tone stays on the dot because amber as text measures 2.7:1 on
            paper — the word carries the meaning, so the colour is redundant. */}
        <span className="mt-[0.15rem] flex w-[5.25rem] shrink-0 items-center gap-1.5">
          <span
            aria-hidden
            className="size-1.5 shrink-0 rounded-full"
            style={{ backgroundColor: severityTone(finding.severity) }}
          />
          <Text variant="label" className="text-foreground">
            {finding.severity}
          </Text>
        </span>
        <span className="flex min-w-0 flex-col gap-0.5">
          <span className="text-sm font-medium">{finding.title}</span>
          <span className="flex flex-wrap items-baseline gap-x-2 gap-y-0.5 text-xs text-muted-foreground">
            <span className="font-mono">{anchorOf(finding)}</span>
            <span aria-hidden className="text-muted-foreground/40">
              ·
            </span>
            <span>{CATEGORY_LABELS[finding.category] ?? finding.category}</span>
            <span aria-hidden className="text-muted-foreground/40">
              ·
            </span>
            {/* Severity is impact if real; confidence is how sure the finder is
                that it IS real. Two axes, so both are stated. */}
            <span>{finding.confidence} confidence</span>
          </span>
        </span>
      </button>

      {open && (
        <div className="flex flex-col gap-3 px-3 pb-3">
          {finding.bodyMd && (
            // Held to a reading measure: the column is wide enough for code
            // blocks, which is far too wide for twenty lines of prose. The
            // finder's claim is the substance here, so it is ink, not muted.
            <div className="max-w-[70ch] text-sm leading-relaxed">
              <Markdown text={finding.bodyMd} />
            </div>
          )}

          {/* Deciding whether a finding is real means looking at the code, and
              acting on it means the thread it became. Both were a manual hunt on
              GitHub until now. The blob link is pinned to the SHA the pass read —
              the branch has moved on, and lines that have shifted are worse than
              no link. */}
          <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-xs">
            <a
              href={blobUrl(review, finding)}
              target="_blank"
              rel="noreferrer"
              className="inline-flex items-center gap-1 text-muted-foreground underline decoration-current/40 underline-offset-4 transition-colors hover:text-foreground"
            >
              <Code2 className="size-3" aria-hidden />
              View the code
            </a>
            {thread && (
              <a
                href={thread}
                target="_blank"
                rel="noreferrer"
                className="inline-flex items-center gap-1 text-muted-foreground underline decoration-current/40 underline-offset-4 transition-colors hover:text-foreground"
              >
                <MessageSquare className="size-3" aria-hidden />
                Open the thread
              </a>
            )}
            {finding.resolution && (
              // The strongest post-hoc trust signal the product has: what the
              // author actually did about it.
              <span className="text-muted-foreground">Author {finding.resolution} this</span>
            )}
          </div>

          {finding.suggestedFix && (
            <div>
              <Text variant="label" tone="muted" className="mb-1 block">
                Suggested fix
              </Text>
              {/* GitHub receives this as a committable suggestion block, so it is
                  replacement code and renders as code here too. */}
              <pre className="overflow-x-auto rounded-md border bg-muted px-3 py-2 font-mono text-[0.8rem]">
                {finding.suggestedFix}
              </pre>
            </div>
          )}

          {finding.evidence.length > 0 && (
            <div>
              <button
                type="button"
                onClick={() => setShowEvidence((v) => !v)}
                aria-expanded={showEvidence}
                className="rounded-md text-xs text-muted-foreground underline decoration-current/40 underline-offset-4 outline-none transition-colors hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring"
              >
                {/* The evidence gate: a finding about a file the finder never read
                    is invalid, so this list is the claim's receipt. */}
                {showEvidence ? "Hide" : "Show"} the {finding.evidence.length}{" "}
                {finding.evidence.length === 1 ? "file" : "files"} the finder read
              </button>
              {showEvidence && (
                <ul className="mt-1 space-y-0.5">
                  {finding.evidence.map((path) => (
                    <li key={path} className="font-mono text-xs text-muted-foreground">
                      {path}
                    </li>
                  ))}
                </ul>
              )}
            </div>
          )}
        </div>
      )}

      {open && verdict && <VerdictBlock verdict={verdict} />}
    </article>
  );
}

/**
 * The verifier's ruling — a separate voice on its own ground, attributed, with its
 * own confidence. Its reasoning must cite code it actually read, so this is the
 * part a reader weighs when deciding whether to act.
 */
function VerdictBlock({ verdict }: { verdict: ReviewVerdict }) {
  const confirmed = verdict.verdict === "confirmed";
  const Icon = confirmed ? ShieldCheck : ShieldX;
  return (
    <div className="flex gap-2 border-t bg-muted/40 px-3 py-2">
      <Icon
        className="mt-0.5 size-3.5 shrink-0"
        style={{
          color: confirmed ? "var(--instrument-nominal)" : "var(--muted-foreground)",
        }}
        aria-hidden
      />
      <div className="flex min-w-0 flex-col gap-0.5">
        <span className="text-sm font-medium">
          Verifier {confirmed ? "confirmed" : "refuted"} this
          <span className="font-normal text-muted-foreground">
            {" "}
            · {verdict.confidence} confidence
          </span>
        </span>
        {/* The trust artifact — it must cite code the verifier actually read —
            so it is set at body size, not as the smallest text on the page. */}
        {verdict.reasoning && (
          <p className="max-w-[70ch] text-sm leading-relaxed text-muted-foreground">
            {verdict.reasoning}
          </p>
        )}
      </div>
    </div>
  );
}
