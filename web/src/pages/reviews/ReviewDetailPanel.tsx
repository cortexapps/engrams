import { Link } from "@tanstack/react-router";
import { ExternalLink, GitPullRequestArrow, Terminal } from "lucide-react";

import type { Review, ReviewFinding, ReviewVerdict } from "../../gen/engram/app/v1/review_pb";
import { useReview } from "../../hooks/useReviews";
import { errorMessage } from "../../lib/errors";
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

function SessionLink({ label, sessionId }: { label: string; sessionId: string }) {
  return (
    <Link
      to="/sessions/$id"
      params={{ id: sessionId }}
      className="inline-flex items-center gap-1 text-xs underline decoration-border underline-offset-4 transition-colors hover:text-foreground"
    >
      <Terminal className="size-3" aria-hidden />
      {label}
    </Link>
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

export function ReviewDetailPanel({ review }: { review: Review }) {
  const { data, isPending, error } = useReview(review.id);

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
  // All findings share the finder session; all verdicts share the verifier session.
  const finderSession = findings[0]?.sessionId;
  const verifierSession = data?.verdicts.find((v) => v.sessionId)?.sessionId;
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
        {finderSession && <SessionLink label="Finder session" sessionId={finderSession} />}
        {verifierSession && <SessionLink label="Verifier session" sessionId={verifierSession} />}
      </div>

      {findings.length === 0 ? (
        <p className="text-sm text-muted-foreground">No findings recorded.</p>
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
