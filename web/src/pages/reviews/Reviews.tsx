import { useState } from "react";
import { Link } from "@tanstack/react-router";
import { timestampDate } from "@bufbuild/protobuf/wkt";
import {
  Ban,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  Clock,
  ExternalLink,
  Loader2,
  XCircle,
} from "lucide-react";

import { PageHeading } from "../../components/page-heading";
import type { Review } from "../../gen/engram/app/v1/review_pb";
import { useNow } from "../../hooks/useNow";
import { useReviews } from "../../hooks/useReviews";
import { errorMessage } from "../../lib/errors";
import { relativeTime } from "../sessions/session-format";
import { cn } from "@/lib/utils";
import { Badge } from "@/components/ui/badge";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { ReviewDetailPanel } from "./ReviewDetailPanel";

// The workflow's durable status → a human-facing stage. Active phases spin; the
// terminal ones carry their own colour so the ledger scans at a glance.
const STAGE: Record<
  string,
  { label: string; icon: typeof Clock; spin?: boolean; className: string }
> = {
  queued: { label: "Queued", icon: Clock, className: "text-muted-foreground" },
  finding: { label: "Finding", icon: Loader2, spin: true, className: "text-sky-500" },
  verifying: { label: "Verifying", icon: Loader2, spin: true, className: "text-amber-500" },
  posted: { label: "Posted", icon: CheckCircle2, className: "text-emerald-500" },
  failed: { label: "Failed", icon: XCircle, className: "text-destructive" },
  halted: { label: "Halted", icon: Ban, className: "text-muted-foreground" },
  superseded: { label: "Superseded", icon: Ban, className: "text-muted-foreground" },
};

function ReviewStage({ status }: { status: string }) {
  const stage = STAGE[status] ?? {
    label: status,
    icon: Clock,
    className: "text-muted-foreground",
  };
  const Icon = stage.icon;
  return (
    <span className={cn("inline-flex items-center gap-1.5 text-sm", stage.className)}>
      <Icon className={cn("size-3.5", stage.spin && "animate-spin")} aria-hidden />
      {stage.label}
    </span>
  );
}

// The session for the phase running right now, so the row can offer a live
// "watch" link without expanding. Empty outside an active phase (or before the
// session id has been stamped).
function liveSession(review: Review): string | undefined {
  if (review.status === "finding") return review.finderSessionId;
  if (review.status === "verifying") return review.verifierSessionId;
  return undefined;
}

function WatchLive({ review }: { review: Review }) {
  const sessionId = liveSession(review);
  if (!sessionId) return null;
  return (
    <Link
      to="/sessions/$id"
      params={{ id: sessionId }}
      onClick={(e) => e.stopPropagation()}
      className="inline-flex items-center gap-1 text-xs text-emerald-600 underline decoration-emerald-500/40 underline-offset-4 transition-colors hover:text-emerald-500 dark:text-emerald-400"
    >
      Watch live
      <span className="relative flex size-1.5" aria-hidden>
        <span className="absolute inline-flex size-full animate-ping rounded-full bg-emerald-500/70" />
        <span className="relative inline-flex size-1.5 rounded-full bg-emerald-500" />
      </span>
    </Link>
  );
}

function CreatedAt({ review, now }: { review: Review; now: number }) {
  if (!review.createdAt) return null;
  const createdAt = timestampDate(review.createdAt);
  return (
    <span title={createdAt.toLocaleString()}>{relativeTime(createdAt.toISOString(), now)} ago</span>
  );
}

function FindingCounts({ review }: { review: Review }) {
  const counts = review.findingCounts;
  if (!counts || counts.total === 0) {
    return <span className="text-muted-foreground">0 findings</span>;
  }

  const severities = [
    ["critical", counts.critical],
    ["high", counts.high],
    ["medium", counts.medium],
    ["low", counts.low],
  ] as const;
  return (
    <div className="flex flex-wrap gap-1.5">
      {severities.map(([severity, count]) =>
        count > 0 ? (
          <Badge key={severity} variant="outline">
            {count} {severity}
          </Badge>
        ) : null,
      )}
    </div>
  );
}

function ReviewRow({ review, now }: { review: Review; now: number }) {
  const [open, setOpen] = useState(false);
  const Chevron = open ? ChevronDown : ChevronRight;
  return (
    <>
      <TableRow className="cursor-pointer" onClick={() => setOpen((v) => !v)} aria-expanded={open}>
        <TableCell className="w-8 pr-0 text-muted-foreground">
          <Chevron className="size-4" aria-hidden />
        </TableCell>
        <TableCell className="font-mono text-xs">{review.repo}</TableCell>
        <TableCell>
          <a
            href={`https://github.com/${review.repo}/pull/${review.prNumber}`}
            target="_blank"
            rel="noreferrer"
            onClick={(e) => e.stopPropagation()}
            className="inline-flex items-center gap-1 underline decoration-border underline-offset-4 transition-colors hover:text-foreground"
          >
            #{review.prNumber}
            <ExternalLink className="size-3" aria-hidden />
          </a>
        </TableCell>
        <TableCell>
          <div className="flex items-center gap-3">
            <ReviewStage status={review.status} />
            <WatchLive review={review} />
          </div>
        </TableCell>
        <TableCell>
          <FindingCounts review={review} />
        </TableCell>
        <TableCell className="font-mono text-xs tabular-nums text-muted-foreground">
          <CreatedAt review={review} now={now} />
        </TableCell>
      </TableRow>
      {open && (
        <TableRow className="hover:bg-transparent">
          <TableCell colSpan={6} className="p-0">
            <ReviewDetailPanel review={review} />
          </TableCell>
        </TableRow>
      )}
    </>
  );
}

export function Reviews() {
  const { data, error, isPending } = useReviews();
  const now = useNow();
  const reviews = data?.reviews ?? [];

  return (
    <div className="flex-1 overflow-auto p-4 md:p-6">
      <div className="flex flex-col gap-6">
        <PageHeading
          title="Reviews"
          eyebrow="Pull requests"
          description="Durable review passes and the findings recorded by finder and verifier sessions. Expand a row to see findings, verdicts, and the sessions that produced them."
        />

        {isPending && <p className="text-sm text-muted-foreground">Loading…</p>}

        {!isPending && error && reviews.length === 0 && (
          <div role="alert" className="rounded-lg border border-dashed p-8 text-center">
            <p className="text-sm text-destructive">Couldn’t load reviews. {errorMessage(error)}</p>
          </div>
        )}

        {!isPending && !error && reviews.length === 0 && (
          <div className="rounded-lg border border-dashed p-8 text-center">
            <p className="text-sm text-muted-foreground">No reviews yet</p>
          </div>
        )}

        {reviews.length > 0 && (
          <div className="rounded-lg border bg-card shadow-xs">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead className="w-8" />
                  <TableHead>Repository</TableHead>
                  <TableHead>PR</TableHead>
                  <TableHead>Stage</TableHead>
                  <TableHead>Findings</TableHead>
                  <TableHead>Created</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {reviews.map((review) => (
                  <ReviewRow key={review.id} review={review} now={now} />
                ))}
              </TableBody>
            </Table>
          </div>
        )}
      </div>
    </div>
  );
}
