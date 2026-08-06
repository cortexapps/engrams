import { ChevronRight, Code2, MessageSquare, ShieldCheck } from "lucide-react";

import type { Review, ReviewFinding, ReviewVerdict } from "../../gen/engram/app/v1/review_pb";
import { Markdown } from "../../components/Markdown";
import { Text } from "@/components/ui/text";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import { cn } from "@/lib/utils";
import { Sep } from "./Sep";
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
 * The group IS the verification status, which is why nothing inside repeats it:
 * everything under "Posted to the pull request" was confirmed, everything under
 * "Refuted by the verifier" was not. The verifier's block therefore carries only
 * what the group cannot — its confidence and its reasoning.
 */
const GROUP_COPY: Record<Outcome, { heading: string; note?: string }> = {
  posted: {
    heading: "Posted to the pull request",
  },
  unresolved: {
    heading: "No decision yet",
    note: "This pass never reached the posting gate.",
  },
  unverified: {
    heading: "Not verified",
    // Tense-neutral on purpose: on a live pass "never judged" would be a lie
    // about a verifier that simply hasn't got there yet.
    note: "An unverified finding never posts.",
  },
  no_anchor: {
    heading: "No line to anchor to",
    note: "About a file, not a line — nowhere to hang a comment.",
  },
  over_cap: {
    heading: "Over the comment cap",
    note: "A pass posts at most ten inline comments.",
  },
  refuted: {
    heading: "Refuted by the verifier",
    note: "The verifier traced these and could not reproduce them.",
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
  const copy = GROUP_COPY[outcome];

  // Sentence case at body weight, not a tracked uppercase label: five outcome
  // groups in eyebrow caps turns the findings into a stack of section banners
  // with the actual findings hiding between them. It is still an h2 — the group
  // IS the reason a finding didn't post, so it has to be reachable by heading —
  // and the button lives INSIDE the heading, which is the disclosure pattern
  // that keeps both the outline and a valid content model.
  return (
    <Collapsible defaultOpen={!collapsible} className="flex flex-col gap-2">
      <div className="flex items-center gap-2">
        {/* A rank above the card titles beneath it. At the cards' own size and
            weight the heading divided nothing — it read as one more finding. */}
        <Text as="h2" variant="heading" className="min-w-0 font-semibold">
          <CollapsibleTrigger className="group flex items-center gap-1.5 rounded-md text-left outline-none transition-colors hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring">
            <ChevronRight
              className="size-3.5 shrink-0 text-muted-foreground transition-transform duration-150 group-data-[state=open]:rotate-90 motion-reduce:transition-none"
              aria-hidden
            />
            {copy.heading}
          </CollapsibleTrigger>
        </Text>
        {/* Beside the heading, not flung to the far edge: a lone figure 700px
            from the words it counts belongs to nothing. */}
        <Badge variant="secondary" className="font-mono font-normal tabular-nums">
          {collapsible ? `${items.length} of ${total}` : items.length}
        </Badge>
      </div>

      <CollapsibleContent className="flex flex-col gap-2">
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
      </CollapsibleContent>
    </Collapsible>
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
 * every body at once is a wall of text.
 *
 * Everything in the card shares ONE left edge: the title leads and severity
 * rides the meta line under it, where the dots still stack into a scannable
 * column. Severity in its own leading column would push the title a third of the
 * way across the card while the body still began at the card's own padding, so a
 * claim and its own heading would have no line to read down.
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
  const thread = threadUrl(review, finding);

  return (
    <Collapsible defaultOpen={defaultOpen} asChild>
      {/* Stock `Card`, run dense: the component owns the edge, the radius and
          the ground, and the overrides only retire its article-sized padding. */}
      <Card className="gap-0 overflow-hidden py-0">
        <CollapsibleTrigger className="group flex w-full flex-col gap-1 px-3 py-2.5 text-left outline-none transition-colors hover:bg-accent/40 focus-visible:bg-accent/40 focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-inset">
          <span className="flex w-full items-start gap-2">
            <span className="min-w-0 flex-1 text-sm font-medium">{finding.title}</span>
            <ChevronRight
              className="mt-0.5 size-3.5 shrink-0 text-muted-foreground transition-transform duration-150 group-data-[state=open]:rotate-90 motion-reduce:transition-none"
              aria-hidden
            />
          </span>
          {/* A dot in the severity's tone, then the word in ink. No pill, so a
              low finding can't shout as loudly as a critical one; and the tone
              stays on the dot because amber as text measures 2.7:1 on paper —
              the word carries the meaning, so the colour is redundant. */}
          <span className="flex flex-wrap items-baseline gap-x-2 gap-y-0.5 text-xs text-muted-foreground">
            <span className="flex items-center gap-1.5 text-foreground">
              <span
                aria-hidden
                className="size-1.5 shrink-0 rounded-full"
                style={{ backgroundColor: severityTone(finding.severity) }}
              />
              {finding.severity}
            </span>
            <Sep />
            <span className="font-mono">{anchorOf(finding)}</span>
            <Sep />
            <span>{CATEGORY_LABELS[finding.category] ?? finding.category}</span>
            <Sep />
            {/* Severity is impact if real; confidence is how sure the finder is
                that it IS real. Two axes, so both are stated. */}
            <span>{finding.confidence} confidence</span>
          </span>
        </CollapsibleTrigger>

        <CollapsibleContent>
          <CardContent className="flex flex-col gap-3 px-3 pt-1 pb-3">
            {finding.bodyMd && (
              // Held to a reading measure: the column is wide enough for code
              // blocks, which is far too wide for twenty lines of prose. The
              // finder's claim is the substance here, so it is ink, not muted.
              <div className="max-w-[70ch] text-sm leading-relaxed">
                <Markdown text={finding.bodyMd} />
              </div>
            )}

            {finding.suggestedFix && (
              <div>
                <Text variant="label" tone="muted" className="mb-1 block">
                  Suggested fix
                </Text>
                {/* GitHub receives this as a committable suggestion block, so it
                    is replacement code and renders as code here too. */}
                <pre className="overflow-x-auto rounded-md border bg-muted px-3 py-2 font-mono text-[0.8rem]">
                  {finding.suggestedFix}
                </pre>
              </div>
            )}

            {finding.evidence.length > 0 && <Evidence paths={finding.evidence} />}

            {/* Only when the verifier said something. Its own ground is for
                reasoning; a box holding "Verifier · medium confidence" and
                nothing else is a container built for three words. */}
            {verdict?.reasoning && <VerdictBlock verdict={verdict} />}

            {/* Deciding whether a finding is real means looking at the code, and
                acting on it means the thread it became. Both were a manual hunt
                on GitHub until now. The blob link is pinned to the SHA the pass
                read — the branch has moved on, and lines that have shifted are
                worse than no link. */}
            <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-xs">
              <CardLink href={blobUrl(review, finding)} icon={Code2}>
                View the code
              </CardLink>
              {thread && (
                <CardLink href={thread} icon={MessageSquare}>
                  Open the thread
                </CardLink>
              )}
              {verdict && !verdict.reasoning && (
                <span className="text-muted-foreground">
                  Verifier · {verdict.confidence} confidence
                </span>
              )}
              {finding.resolution && (
                // The strongest post-hoc trust signal the product has: what the
                // author actually did about it.
                <Badge variant="secondary" className="font-normal text-muted-foreground">
                  Author {finding.resolution} this
                </Badge>
              )}
            </div>
          </CardContent>
        </CollapsibleContent>
      </Card>
    </Collapsible>
  );
}

function CardLink({
  href,
  icon: Icon,
  children,
}: {
  href: string;
  icon: typeof Code2;
  children: React.ReactNode;
}) {
  return (
    <a
      href={href}
      target="_blank"
      rel="noreferrer"
      className="inline-flex items-center gap-1 text-muted-foreground underline decoration-current/40 underline-offset-4 transition-colors hover:text-foreground"
    >
      <Icon className="size-3" aria-hidden />
      {children}
    </a>
  );
}

/** The evidence gate: a finding about a file the finder never read is invalid,
 *  so this list is the claim's receipt. */
function Evidence({ paths }: { paths: readonly string[] }) {
  return (
    <Collapsible>
      <CollapsibleTrigger className="group flex items-center gap-1 rounded-md text-xs text-muted-foreground outline-none transition-colors hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring">
        <ChevronRight
          className="size-3 transition-transform duration-150 group-data-[state=open]:rotate-90 motion-reduce:transition-none"
          aria-hidden
        />
        {paths.length} {paths.length === 1 ? "file" : "files"} the finder read
      </CollapsibleTrigger>
      <CollapsibleContent>
        <ul className="mt-1 space-y-0.5 pl-4">
          {paths.map((path) => (
            <li key={path} className="font-mono text-xs text-muted-foreground">
              {path}
            </li>
          ))}
        </ul>
      </CollapsibleContent>
    </Collapsible>
  );
}

/**
 * The verifier's ruling — a separate voice on its own ground, attributed, with its
 * own confidence. Its reasoning must cite code it actually read, so this is the
 * part a reader weighs when deciding whether to act.
 *
 * It does NOT repeat the verdict: the group heading already says whether these
 * findings were confirmed or refuted, and a band across every card restating it
 * was three-quarters of the noise in the old ledger. What only the verifier can
 * tell you is how sure it was, and why.
 */
function VerdictBlock({ verdict }: { verdict: ReviewVerdict }) {
  const confirmed = verdict.verdict === "confirmed";
  return (
    <div className="flex gap-2 rounded-md border bg-muted/40 px-3 py-2">
      <ShieldCheck
        className={cn("mt-0.5 size-3.5 shrink-0", !confirmed && "opacity-50")}
        style={{ color: confirmed ? "var(--instrument-nominal)" : "var(--muted-foreground)" }}
        aria-hidden
      />
      <div className="flex min-w-0 flex-col gap-0.5">
        <span className="text-xs text-muted-foreground">
          Verifier · {verdict.confidence} confidence
        </span>
        {/* The trust artifact — it must cite code the verifier actually read —
            so it is set at body size, not as the smallest text on the page. */}
        {verdict.reasoning && (
          <p className="max-w-[70ch] text-sm leading-relaxed">{verdict.reasoning}</p>
        )}
      </div>
    </div>
  );
}
