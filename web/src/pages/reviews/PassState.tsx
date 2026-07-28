import { timestampDate } from "@bufbuild/protobuf/wkt";
import {
  Ban,
  Bot,
  CheckCircle2,
  ChevronDown,
  Clock,
  GitBranch,
  ScanSearch,
  ShieldCheck,
  XCircle,
} from "lucide-react";

import type { Review, ReviewEvent } from "../../gen/engram/app/v1/review_pb";
import { LivePulse } from "../../components/LivePulse";
import { Text } from "@/components/ui/text";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { cn } from "@/lib/utils";
import { ReviewStage } from "./ReviewGlyph";
import { isActive, passDuration, shortDuration, stageOf } from "./review-format";

/**
 * What the pass is doing, or what it ended up doing — and the log behind it.
 *
 * State and activity are one idea at two zoom levels, not two controls. `posted`,
 * `failed` and `halted` are simultaneously review statuses and event kinds: a
 * terminal pass's state IS the last line of its own log. So the state is the
 * control, and clicking it opens the log it summarises.
 *
 * Live, it reports the finest resolution available — the step, "Reviewing
 * changes", which says more than the stage word "Finding" — and breathes. Terminal,
 * it reports the stage with the step count behind it. With no log at all it stays
 * a plain readout rather than a button that opens nothing.
 */
export function PassState({
  review,
  events,
  loading,
}: {
  review: Review;
  events: ReviewEvent[];
  loading: boolean;
}) {
  const live = isActive(review);

  if (loading && events.length === 0) return <Skeleton className="h-7 w-40" />;

  // No milestones recorded yet: still say what the pass is, because a row that
  // silently loses its state is indistinguishable from one that failed to load.
  if (events.length === 0) {
    return (
      <span className="inline-flex items-center gap-2 px-1 text-sm font-medium">
        <ReviewStage status={review.status} />
        {live && <LivePulse />}
      </span>
    );
  }

  // Non-null: length is checked above.
  const current = events[events.length - 1]!;
  const step = eventMeta(current.kind);
  const StepIcon = step.icon;
  const duration = passDuration(events);

  return (
    <Popover>
      <PopoverTrigger asChild>
        {/* Ghost, against the pass switcher's outline: a border means "this changes
            what you are looking at", no border means "this reveals more about it". */}
        <Button
          variant="ghost"
          size="sm"
          className="-ml-2 shrink-0 gap-1.5 text-sm font-medium"
          aria-label={`${live ? step.label : stageOf(review.status).label} — show the activity log`}
        >
          {live ? (
            <>
              <StepIcon
                className="size-3.5 animate-pulse motion-reduce:animate-none"
                style={{ color: "var(--instrument-caution)" }}
                aria-hidden
              />
              {step.label}
              <LivePulse />
            </>
          ) : (
            <>
              <ReviewStage status={review.status} />
              <span className="font-mono text-xs font-normal tabular-nums text-muted-foreground">
                {events.length}
              </span>
            </>
          )}
          <ChevronDown className="size-3.5 text-muted-foreground" aria-hidden />
        </Button>
      </PopoverTrigger>
      <PopoverContent align="start" className="w-96 p-0">
        <div className="flex items-baseline justify-between gap-4 border-b px-3 py-2">
          <Text variant="label" tone="muted">
            Activity
          </Text>
          {duration && (
            <span className="font-mono text-xs tabular-nums text-muted-foreground">{duration}</span>
          )}
        </div>
        <ol className="flex flex-col gap-1.5 p-2">
          {events.map((event, i) => {
            const entry = eventMeta(event.kind);
            const Icon = entry.icon;
            const running = i === events.length - 1 && live;
            const at = event.createdAt ? timestampDate(event.createdAt) : undefined;
            const next = events[i + 1]?.createdAt;
            // Per-step duration from adjacent entries. The last entry has nothing
            // after it to measure against, so it simply has no duration — the wall
            // clock it used to fall back to put "4:14:53 PM" in a column of spans.
            const span =
              at && next ? shortDuration(timestampDate(next).getTime() - at.getTime()) : undefined;

            return (
              // A grid, not a baseline flex row: as flex siblings the label and the
              // detail both shrank and wrapped into each other, and the duration
              // column drifted with them.
              <li
                key={event.id}
                className="grid grid-cols-[1rem_1fr_auto] items-start gap-x-2.5"
                title={at?.toLocaleTimeString()}
              >
                <Icon
                  className={cn(
                    "mt-[0.2rem] size-3.5",
                    running && "animate-pulse motion-reduce:animate-none",
                  )}
                  style={{ color: entry.tone ?? "var(--muted-foreground)" }}
                  aria-hidden
                />
                <div className="min-w-0">
                  <div className={cn("text-sm leading-snug", running && "font-medium")}>
                    {entry.label}
                  </div>
                  {/* Its own line: "1 candidate finding" is a different kind of fact
                      from the milestone, and sharing a line made both of them wrap. */}
                  {event.detail && (
                    <div className="text-xs leading-snug text-muted-foreground">{event.detail}</div>
                  )}
                </div>
                <span className="mt-px flex shrink-0 items-center gap-1.5 font-mono text-xs tabular-nums text-muted-foreground">
                  {running ? (
                    <>
                      now
                      <LivePulse />
                    </>
                  ) : (
                    span
                  )}
                </span>
              </li>
            );
          })}
        </ol>
      </PopoverContent>
    </Popover>
  );
}

// Milestones the control plane records → a human label and an icon. Only the
// terminal ones carry an instrument tone; the working steps stay muted so the log
// reads as a sequence rather than as five competing signals.
const EVENT: Record<string, { label: string; icon: typeof Clock; tone?: string }> = {
  queued: { label: "Queued", icon: Clock },
  finder_started: { label: "Finder session started", icon: Bot },
  cloning: { label: "Cloning repository", icon: GitBranch },
  reviewing: { label: "Reviewing changes", icon: ScanSearch },
  verifier_started: { label: "Verifier session started", icon: Bot },
  verifying: { label: "Verifying findings", icon: ShieldCheck },
  posted: { label: "Posted review", icon: CheckCircle2, tone: "var(--instrument-nominal)" },
  failed: { label: "Failed", icon: XCircle, tone: "var(--instrument-critical)" },
  halted: { label: "Halted", icon: Ban },
};

/** An unrecognised milestone still renders as itself rather than vanishing. */
function eventMeta(kind: string) {
  return EVENT[kind] ?? { label: kind, icon: Clock };
}
