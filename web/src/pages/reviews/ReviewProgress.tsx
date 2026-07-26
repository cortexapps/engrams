import { useState } from "react";
import { Link } from "@tanstack/react-router";
import { timestampDate } from "@bufbuild/protobuf/wkt";
import {
  Ban,
  Bot,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  Clock,
  GitBranch,
  ScanSearch,
  ShieldCheck,
  Terminal,
  XCircle,
} from "lucide-react";

import type { Review, ReviewEvent } from "../../gen/engram/app/v1/review_pb";
import { useNow } from "../../hooks/useNow";
import { relativeTime } from "../sessions/session-format";
import { LivePulse } from "../../components/LivePulse";
import { Text } from "@/components/ui/text";
import { Skeleton } from "@/components/ui/skeleton";
import { cn } from "@/lib/utils";
import { ReviewStage } from "./ReviewGlyph";
import { keptRatio, type JudgedFinding } from "./review-findings";
import { isActive, reviewCreatedAt } from "./review-format";

/**
 * Progress has two weights, and it does not move between them.
 *
 * While a pass is queued/finding/verifying there are no settled findings yet and
 * progress IS the content: the activity log is open, full width, with the
 * current step live. Once the pass is terminal, findings are the content and the
 * log collapses to a single line — step count, total duration, per-phase split —
 * that expands in place. Attention goes where the state actually is.
 */
export function ReviewProgress({
  review,
  events,
  judged,
  loading,
}: {
  review: Review;
  events: ReviewEvent[];
  judged: JudgedFinding[];
  loading: boolean;
}) {
  const live = isActive(review.status);
  // Live passes open the log; terminal ones start collapsed and remember the
  // reader's choice for as long as the dossier is mounted.
  const [expanded, setExpanded] = useState(live);
  const open = live || expanded;
  const Chevron = open ? ChevronDown : ChevronRight;

  return (
    <section className="flex flex-col gap-2">
      <VerdictLine review={review} judged={judged} />

      {loading && events.length === 0 ? (
        <Skeleton className="h-4 w-48" />
      ) : events.length === 0 ? null : live ? (
        <ReviewLog review={review} events={events} active />
      ) : (
        <>
          <button
            type="button"
            onClick={() => setExpanded((v) => !v)}
            aria-expanded={expanded}
            className="flex w-fit items-center gap-1.5 text-left text-xs text-muted-foreground transition-colors hover:text-foreground"
          >
            <Chevron className="size-3.5" aria-hidden />
            <span className="font-mono tabular-nums">{summarise(events)}</span>
          </button>
          {expanded && <ReviewLog review={review} events={events} active={false} />}
        </>
      )}
    </section>
  );
}

/**
 * The pass's conclusion in one line, carrying the kept-vs-killed ratio — the
 * trust signal the previous list page showed nowhere. A reader who stops here
 * still leaves knowing whether to believe the output.
 */
function VerdictLine({ review, judged }: { review: Review; judged: JudgedFinding[] }) {
  const ratio = keptRatio(judged);
  const at = reviewCreatedAt(review);
  const now = useNow();
  const live = isActive(review.status);

  return (
    <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
      <span className="inline-flex items-baseline gap-2">
        <ReviewStage status={review.status} className="text-sm font-medium" />
        {live && <LivePulse />}
      </span>
      {!live && review.status === "posted" && (
        <span className="text-sm text-muted-foreground">
          {ratio.total === 0
            ? "Nothing to report."
            : ratio.refuted > 0
              ? `${ratio.kept} of ${ratio.total} kept · verifier refuted ${ratio.refuted}`
              : `${ratio.kept} ${ratio.kept === 1 ? "finding" : "findings"} kept`}
          {ratio.posted > 0 && ` · ${ratio.posted} posted to the PR`}
        </span>
      )}
      {at && (
        <span
          title={at.toLocaleString()}
          className="ml-auto font-mono text-xs tabular-nums text-muted-foreground"
        >
          started {relativeTime(at.toISOString(), now)} ago
        </span>
      )}
    </div>
  );
}

// Milestones the control plane records → a human label, an icon, and (for the
// two that name a worker session) which role's transcript they open. The log is
// the way into the transcripts: a step that names a session is the click target
// for it. Control-plane moments have no session behind them and stay inert.
const EVENT: Record<
  string,
  { label: string; icon: typeof Clock; role?: "finder" | "verifier"; tone?: string }
> = {
  queued: { label: "Queued", icon: Clock },
  finder_started: { label: "Finder session started", icon: Bot, role: "finder" },
  cloning: { label: "Cloning repository", icon: GitBranch, role: "finder" },
  reviewing: { label: "Reviewing changes", icon: ScanSearch, role: "finder" },
  verifier_started: { label: "Verifier session started", icon: Bot, role: "verifier" },
  verifying: { label: "Verifying findings", icon: ShieldCheck, role: "verifier" },
  posted: { label: "Posted review", icon: CheckCircle2, tone: "var(--instrument-nominal)" },
  failed: { label: "Failed", icon: XCircle, tone: "var(--instrument-critical)" },
  halted: { label: "Halted", icon: Ban },
};

/** Human span between two log entries: "12s", "3m 4s", "1h 2m". */
function shortDuration(ms: number): string {
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

/** The collapsed weight: "6 steps · 4m 12s". Durations still come from adjacent
 *  log entries, so no wall clock is consulted. */
function summarise(events: ReviewEvent[]): string {
  const steps = `${events.length} ${events.length === 1 ? "step" : "steps"}`;
  const first = events[0]?.createdAt;
  const last = events[events.length - 1]?.createdAt;
  if (!first || !last) return steps;
  const span = timestampDate(last).getTime() - timestampDate(first).getTime();
  if (span <= 0) return steps;
  return `${steps} · ${shortDuration(span)}`;
}

function ReviewLog({
  review,
  events,
  active,
}: {
  review: Review;
  events: ReviewEvent[];
  active: boolean;
}) {
  // The log is the way into the transcripts: a milestone that names a worker
  // session is the click target for that session's thread. The ids are stamped
  // at kickoff and outlive the session itself, so a finished pass still links.
  const sessionFor = (role: "finder" | "verifier" | undefined): string | undefined => {
    if (role === "finder") return review.finderSessionId;
    if (role === "verifier") return review.verifierSessionId;
    return undefined;
  };

  return (
    <div>
      <Text variant="label" tone="muted" className="mb-2 block">
        Activity
      </Text>
      <ol className="space-y-1">
        {events.map((event, i) => {
          const meta = EVENT[event.kind] ?? { label: event.kind, icon: Clock };
          const Icon = meta.icon;
          const isLast = i === events.length - 1;
          const running = isLast && active;
          const at = event.createdAt ? timestampDate(event.createdAt) : undefined;
          const next = events[i + 1]?.createdAt;
          // Per-step duration from adjacent entries; the last entry is either
          // still running or has just finished.
          const span =
            at && next ? shortDuration(timestampDate(next).getTime() - at.getTime()) : undefined;

          const sessionId = sessionFor(meta.role);

          return (
            <li key={event.id}>
              <StepRow
                sessionId={sessionId}
                className={cn(
                  "flex items-center gap-2 rounded-md px-2 py-1 text-sm",
                  sessionId &&
                    "-mx-2 transition-colors hover:bg-accent/60 focus-visible:bg-accent/60 focus-visible:ring-2 focus-visible:ring-ring/50 focus-visible:outline-none",
                )}
              >
                <Icon
                  className={cn(
                    "size-3.5 shrink-0",
                    running && "animate-pulse motion-reduce:animate-none",
                  )}
                  style={{ color: meta.tone ?? "var(--muted-foreground)" }}
                  aria-hidden
                />
                <span className={cn(running && "font-medium")}>{meta.label}</span>
                {event.detail && (
                  <span className="truncate text-xs text-muted-foreground">· {event.detail}</span>
                )}
                <span className="ml-auto flex shrink-0 items-center gap-1.5 font-mono text-xs tabular-nums text-muted-foreground">
                  {running ? (
                    <>
                      in progress
                      <LivePulse />
                    </>
                  ) : span ? (
                    span
                  ) : at ? (
                    at.toLocaleTimeString()
                  ) : null}
                  {sessionId && (
                    <Terminal className="size-3 text-muted-foreground/70" aria-hidden />
                  )}
                </span>
              </StepRow>
            </li>
          );
        })}
      </ol>
    </div>
  );
}

/**
 * A milestone that names a worker session opens that session; one that doesn't
 * renders as plain text. It links to the full session page today — the side pane
 * is a sibling change, and this is the same destination one hop further out, so
 * the affordance is real rather than promised.
 */
function StepRow({
  sessionId,
  className,
  children,
}: {
  sessionId: string | undefined;
  className?: string;
  children: React.ReactNode;
}) {
  if (!sessionId) {
    return <div className={className}>{children}</div>;
  }
  return (
    <Link to="/sessions/$id" params={{ id: sessionId }} className={className}>
      {children}
    </Link>
  );
}
