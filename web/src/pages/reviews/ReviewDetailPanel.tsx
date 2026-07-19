import { Link } from "@tanstack/react-router";
import { timestampDate } from "@bufbuild/protobuf/wkt";
import {
  Ban,
  Bot,
  CheckCircle2,
  Clock,
  ExternalLink,
  GitBranch,
  GitPullRequestArrow,
  ScanSearch,
  ShieldCheck,
  Terminal,
  XCircle,
} from "lucide-react";

import type {
  Review,
  ReviewEvent,
  ReviewFinding,
  ReviewVerdict,
} from "../../gen/engram/app/v1/review_pb";
import { useReview } from "../../hooks/useReviews";
import { errorMessage } from "../../lib/errors";
import { cn } from "@/lib/utils";
import { Badge } from "@/components/ui/badge";
import { Skeleton } from "@/components/ui/skeleton";

const SEVERITY_ORDER: Record<string, number> = {
  critical: 0,
  high: 1,
  medium: 2,
  low: 3,
};

// Severity tint. Kept deliberately close to the finding-counts badges so the
// list and the detail read as the same vocabulary.
function severityClass(severity: string): string {
  switch (severity) {
    case "critical":
      return "border-red-500/40 bg-red-500/10 text-red-600 dark:text-red-400";
    case "high":
      return "border-orange-500/40 bg-orange-500/10 text-orange-600 dark:text-orange-400";
    case "medium":
      return "border-amber-500/40 bg-amber-500/10 text-amber-600 dark:text-amber-400";
    default:
      return "border-sky-500/40 bg-sky-500/10 text-sky-600 dark:text-sky-400";
  }
}

// A finding's lifecycle state, phrased for a human reading the ledger.
const STATE_LABELS: Record<string, string> = {
  candidate: "candidate",
  confirmed: "confirmed",
  posted: "posted to PR",
  ui_only: "shown here only",
  suppressed_refuted: "refuted",
  suppressed_by_config: "suppressed",
  superseded: "superseded",
};

function anchor(finding: ReviewFinding): string {
  const start = finding.startLine;
  const end = finding.endLine;
  if (start && end && start !== end) return `${finding.path}:L${start}-L${end}`;
  const line = end ?? start;
  return line ? `${finding.path}:L${line}` : finding.path;
}

function SessionLink({
  label,
  sessionId,
  live,
}: {
  label: string;
  sessionId: string;
  live?: boolean;
}) {
  return (
    <Link
      to="/sessions/$id"
      params={{ id: sessionId }}
      className="inline-flex items-center gap-1 text-xs underline decoration-border underline-offset-4 transition-colors hover:text-foreground"
    >
      <Terminal className="size-3" aria-hidden />
      {live ? `Watch ${label.toLowerCase()} live` : label}
      {live && (
        <span className="relative flex size-1.5" aria-hidden>
          <span className="absolute inline-flex size-full animate-ping rounded-full bg-emerald-500/70" />
          <span className="relative inline-flex size-1.5 rounded-full bg-emerald-500" />
        </span>
      )}
    </Link>
  );
}

// Activity-log milestones the control plane records → a human label + icon.
// `terminal` colours the final outcome so the log scans at a glance.
const EVENT: Record<string, { label: string; icon: typeof Clock; className?: string }> = {
  queued: { label: "Queued", icon: Clock },
  finder_started: { label: "Finder session started", icon: Bot },
  cloning: { label: "Cloning repository", icon: GitBranch },
  reviewing: { label: "Reviewing changes", icon: ScanSearch },
  verifier_started: { label: "Verifier session started", icon: Bot },
  verifying: { label: "Verifying findings", icon: ShieldCheck },
  posted: { label: "Posted review", icon: CheckCircle2, className: "text-emerald-500" },
  failed: { label: "Failed", icon: XCircle, className: "text-destructive" },
  halted: { label: "Halted", icon: Ban, className: "text-muted-foreground" },
};

// Short, human span between two log entries: "12s", "3m 4s", "1h 2m".
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

function ReviewLog({ events, active }: { events: ReviewEvent[]; active: boolean }) {
  if (events.length === 0) return null;
  return (
    <div>
      <h4 className="mb-2 text-xs font-medium uppercase tracking-wide text-muted-foreground">
        Activity
      </h4>
      <ol className="space-y-1.5">
        {events.map((event, i) => {
          const meta = EVENT[event.kind] ?? { label: event.kind, icon: Clock };
          const Icon = meta.icon;
          const isLast = i === events.length - 1;
          const running = isLast && active;
          const at = event.createdAt ? timestampDate(event.createdAt) : undefined;
          const next = events[i + 1]?.createdAt;
          // Per-step duration comes from adjacent log entries (no wall clock
          // needed); the last entry is either still running or just done.
          const span =
            at && next ? shortDuration(timestampDate(next).getTime() - at.getTime()) : undefined;
          return (
            <li key={event.id} className="flex items-center gap-2 text-sm">
              <Icon
                className={cn(
                  "size-3.5 shrink-0",
                  meta.className ?? "text-muted-foreground",
                  running && "animate-pulse",
                )}
                aria-hidden
              />
              <span className={cn(running ? "font-medium" : undefined)}>{meta.label}</span>
              {event.detail && (
                <span className="text-xs text-muted-foreground">· {event.detail}</span>
              )}
              <span className="ml-auto font-mono text-xs tabular-nums text-muted-foreground">
                {running ? (
                  <span className="inline-flex items-center gap-1 text-emerald-600 dark:text-emerald-400">
                    in progress
                    <span className="relative flex size-1.5" aria-hidden>
                      <span className="absolute inline-flex size-full animate-ping rounded-full bg-emerald-500/70" />
                      <span className="relative inline-flex size-1.5 rounded-full bg-emerald-500" />
                    </span>
                  </span>
                ) : span ? (
                  span
                ) : at ? (
                  at.toLocaleTimeString()
                ) : null}
              </span>
            </li>
          );
        })}
      </ol>
    </div>
  );
}

function FindingCard({
  finding,
  verdict,
}: {
  finding: ReviewFinding;
  verdict: ReviewVerdict | undefined;
}) {
  return (
    <div className="rounded-lg border bg-background p-3">
      <div className="flex flex-wrap items-center gap-2">
        <Badge variant="outline" className={severityClass(finding.severity)}>
          {finding.severity}
        </Badge>
        <span className="text-sm font-medium">{finding.title}</span>
      </div>
      <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-muted-foreground">
        <span className="font-mono">{anchor(finding)}</span>
        <span>{finding.category}</span>
        <Badge variant="secondary" className="text-[10px]">
          {STATE_LABELS[finding.state] ?? finding.state}
        </Badge>
      </div>
      {finding.bodyMd && (
        <p className="mt-2 whitespace-pre-wrap text-sm text-muted-foreground">{finding.bodyMd}</p>
      )}
      {verdict && (
        <div className="mt-2 border-l-2 border-border pl-3 text-xs">
          <span className="font-medium">
            Verifier: {verdict.verdict}
            {verdict.confidence ? ` (${verdict.confidence} confidence)` : ""}
          </span>
          {verdict.reasoning && (
            <p className="mt-0.5 whitespace-pre-wrap text-muted-foreground">{verdict.reasoning}</p>
          )}
        </div>
      )}
    </div>
  );
}

const ACTIVE_STATUSES: ReadonlySet<string> = new Set(["queued", "finding", "verifying"]);

export function ReviewDetailPanel({ review }: { review: Review }) {
  const active = ACTIVE_STATUSES.has(review.status);
  const { data, isPending, error } = useReview(review.id, { active });

  if (isPending) {
    return (
      <div className="space-y-2 p-4">
        <Skeleton className="h-4 w-40" />
        <Skeleton className="h-16 w-full" />
      </div>
    );
  }
  if (error) {
    return (
      <p className="p-4 text-sm text-destructive">
        couldn’t load this review — {errorMessage(error)}
      </p>
    );
  }

  const findings = [...(data?.findings ?? [])].sort(
    (a, b) => (SEVERITY_ORDER[a.severity] ?? 9) - (SEVERITY_ORDER[b.severity] ?? 9),
  );
  const verdictByFinding = new Map((data?.verdicts ?? []).map((v) => [v.findingId, v]));
  // Prefer the session ids stamped on the review at kickoff (present before any
  // finding/verdict exists, so the link shows the moment a phase starts); fall
  // back to the producing session recorded on a finding/verdict.
  const finderSession = review.finderSessionId || findings[0]?.sessionId;
  const verifierSession =
    review.verifierSessionId || data?.verdicts.find((v) => v.sessionId)?.sessionId;
  const reviewUrl = review.githubReviewId
    ? `https://github.com/${review.repo}/pull/${review.prNumber}#pullrequestreview-${review.githubReviewId}`
    : undefined;

  return (
    <div className="space-y-4 border-t bg-muted/30 p-4">
      <div className="flex flex-wrap items-center gap-x-4 gap-y-2">
        <a
          href={`https://github.com/${review.repo}/pull/${review.prNumber}`}
          target="_blank"
          rel="noreferrer"
          className="inline-flex items-center gap-1 text-xs underline decoration-border underline-offset-4 transition-colors hover:text-foreground"
        >
          <GitPullRequestArrow className="size-3" aria-hidden />
          Pull request #{review.prNumber}
        </a>
        {reviewUrl && (
          <a
            href={reviewUrl}
            target="_blank"
            rel="noreferrer"
            className="inline-flex items-center gap-1 text-xs underline decoration-border underline-offset-4 transition-colors hover:text-foreground"
          >
            <ExternalLink className="size-3" aria-hidden />
            Review on GitHub
          </a>
        )}
        {finderSession && (
          <SessionLink
            label="Finder session"
            sessionId={finderSession}
            live={review.status === "finding"}
          />
        )}
        {verifierSession && (
          <SessionLink
            label="Verifier session"
            sessionId={verifierSession}
            live={review.status === "verifying"}
          />
        )}
      </div>

      <ReviewLog events={data?.events ?? []} active={active} />

      {findings.length === 0 ? (
        <p className="text-sm text-muted-foreground">
          {active ? "No findings yet." : "No findings recorded."}
        </p>
      ) : (
        <div className="space-y-2">
          {findings.map((finding) => (
            <FindingCard
              key={finding.id}
              finding={finding}
              verdict={verdictByFinding.get(finding.id)}
            />
          ))}
        </div>
      )}
    </div>
  );
}
