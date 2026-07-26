import { useState } from "react";
import { ChevronDown, ChevronRight, ShieldCheck, ShieldX } from "lucide-react";

import type { Review, ReviewFinding, ReviewVerdict } from "../../gen/engram/app/v1/review_pb";
import { Markdown } from "../../components/Markdown";
import { Text } from "@/components/ui/text";
import { Skeleton } from "@/components/ui/skeleton";
import { cn } from "@/lib/utils";
import {
  groupByOutcome,
  isUnresolvedPass,
  type JudgedFinding,
  type Outcome,
} from "./review-findings";
import { severityTone } from "./review-format";

/**
 * The findings, grouped by what actually happened to them.
 *
 * Severity answers "how bad"; outcome answers the question a reader asks first —
 * did this reach the PR, and if not, why not. `ui_only` is one stored state
 * meaning four different things, so the reason is derived and named here rather
 * than collapsed into "shown here only" (ADR 0100).
 */
const GROUP_COPY: Record<Outcome, { heading: string; note?: string }> = {
  posted: {
    heading: "Posted to the pull request",
  },
  unverified: {
    heading: "Not verified",
    note: "The verifier never judged these, and an unverified finding never posts — silence counts as low confidence, not as agreement.",
  },
  no_anchor: {
    heading: "No line to anchor to",
    note: "Confirmed, but about a file rather than a specific line, so there is nowhere to hang an inline comment.",
  },
  over_cap: {
    heading: "Over the comment cap",
    note: "Confirmed and anchored, but a pass posts at most ten inline comments; these are the overflow.",
  },
  refuted: {
    heading: "Refuted by the verifier",
    note: "The verifier traced these and could not reproduce the failure. Kept as a record of what was considered and dropped.",
  },
};

export function FindingsLedger({
  review,
  judged,
  loading,
}: {
  review: Review;
  judged: JudgedFinding[];
  loading: boolean;
}) {
  if (loading && judged.length === 0) {
    return (
      <section className="flex flex-col gap-2">
        <Text variant="label" tone="muted">
          Findings
        </Text>
        <Skeleton className="h-24 w-full" />
      </section>
    );
  }

  if (judged.length === 0) {
    return (
      <section className="flex flex-col gap-2">
        <Text variant="label" tone="muted">
          Findings
        </Text>
        <p className="rounded-lg border border-dashed px-3 py-6 text-center text-sm text-muted-foreground">
          {isUnresolvedPass(review)
            ? "Nothing reported yet."
            : "The finder reported nothing on this change."}
        </p>
      </section>
    );
  }

  const groups = groupByOutcome(judged);

  return (
    <section className="flex flex-col gap-4">
      <div className="flex items-baseline justify-between gap-4 border-b pb-2">
        <Text variant="label" tone="muted">
          Findings
        </Text>
        <span className="font-mono text-xs tabular-nums text-muted-foreground">
          {judged.length}
        </span>
      </div>

      {/* A pass that never reached posting leaves every finding at `candidate`,
          because only the posting step advances them. Saying "over the cap" for
          those would be a lie about why they aren't on the PR. */}
      {isUnresolvedPass(review) && (
        <p className="text-xs text-muted-foreground">
          This pass didn’t finish, so nothing here has been through the posting gate yet.
        </p>
      )}

      {groups.map((group) => (
        <OutcomeSection
          key={group.outcome}
          outcome={group.outcome}
          items={group.items}
          total={judged.length}
        />
      ))}
    </section>
  );
}

/** Refuted findings start collapsed: they are the verifier's working, not the
 *  pass's output, and the count is the part that matters at a glance. */
function OutcomeSection({
  outcome,
  items,
  total,
}: {
  outcome: Outcome;
  items: JudgedFinding[];
  total: number;
}) {
  const collapsible = outcome === "refuted";
  const [open, setOpen] = useState(!collapsible);
  const copy = GROUP_COPY[outcome];
  const Chevron = open ? ChevronDown : ChevronRight;

  const heading = (
    <>
      <Text variant="label" className="text-foreground">
        {copy.heading}
      </Text>
      <span className="ml-auto shrink-0 font-mono text-xs tabular-nums text-muted-foreground">
        {collapsible ? `${items.length} of ${total}` : items.length}
      </span>
    </>
  );

  return (
    <div className="flex flex-col gap-2">
      {collapsible ? (
        <button
          type="button"
          onClick={() => setOpen((v) => !v)}
          aria-expanded={open}
          className="flex w-full items-baseline gap-1.5 text-left transition-colors hover:text-foreground"
        >
          <Chevron className="size-3.5 shrink-0 text-muted-foreground" aria-hidden />
          {heading}
        </button>
      ) : (
        <div className="flex items-baseline gap-1.5">{heading}</div>
      )}

      {open && (
        <>
          {copy.note && <p className="text-xs text-muted-foreground">{copy.note}</p>}
          <ul className="flex flex-col gap-2">
            {items.map((item) => (
              <li key={item.finding.id}>
                <FindingCard finding={item.finding} verdict={item.verdict} />
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
 * Two voices, never blended: the finder's claim is the body, the verifier's
 * ruling is a separate attributed block below it. Reading them as one paragraph
 * is what made the old panel untrustworthy — you couldn't tell which of them was
 * making a given assertion.
 */
function FindingCard({
  finding,
  verdict,
}: {
  finding: ReviewFinding;
  verdict: ReviewVerdict | undefined;
}) {
  const [showEvidence, setShowEvidence] = useState(false);

  return (
    <article className="rounded-lg border bg-card">
      <div className="flex flex-col gap-1.5 p-3">
        <div className="flex flex-wrap items-baseline gap-x-2 gap-y-1">
          {/* Severity as a word in its own tone — no pill, so a low finding
              can't shout as loudly as a critical one. */}
          <span
            className="font-display text-[0.7rem] font-medium uppercase tracking-[0.1em]"
            style={{ color: severityTone(finding.severity) }}
          >
            {finding.severity}
          </span>
          <span className="text-sm font-medium">{finding.title}</span>
        </div>

        <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1 text-xs text-muted-foreground">
          <span className="font-mono">{anchorOf(finding)}</span>
          <span>{CATEGORY_LABELS[finding.category] ?? finding.category}</span>
          {/* Severity is impact if real; confidence is how sure the finder is
              that it IS real. Two axes, so both are stated. */}
          <span>{finding.confidence} confidence</span>
        </div>

        {finding.bodyMd && (
          <div className="text-sm leading-relaxed text-muted-foreground">
            <Markdown text={finding.bodyMd} />
          </div>
        )}

        {finding.suggestedFix && (
          <div className="mt-1">
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
              className="text-xs text-muted-foreground underline decoration-border underline-offset-4 transition-colors hover:text-foreground"
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

      {verdict && <VerdictBlock verdict={verdict} />}
    </article>
  );
}

/**
 * The verifier's ruling — a separate voice on its own ground, attributed, with
 * its own confidence. Its reasoning must cite code it actually read, so this is
 * the part a reader weighs when deciding whether to act.
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
        <span className="text-xs font-medium">
          Verifier {confirmed ? "confirmed" : "refuted"} this
          <span className="font-normal text-muted-foreground">
            {" "}
            · {verdict.confidence} confidence
          </span>
        </span>
        {verdict.reasoning && (
          <p className={cn("text-xs leading-relaxed text-muted-foreground")}>{verdict.reasoning}</p>
        )}
      </div>
    </div>
  );
}
