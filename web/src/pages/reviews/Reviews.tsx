import { timestampDate } from "@bufbuild/protobuf/wkt";
import { ExternalLink } from "lucide-react";

import { PageHeading } from "../../components/page-heading";
import type { Review } from "../../gen/engram/app/v1/review_pb";
import { useNow } from "../../hooks/useNow";
import { useReviews } from "../../hooks/useReviews";
import { errorMessage } from "../../lib/errors";
import { relativeTime } from "../sessions/session-format";
import { Badge } from "@/components/ui/badge";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";

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
          description="Durable review passes and the findings recorded by finder and verifier sessions."
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
                  <TableHead>Repository</TableHead>
                  <TableHead>PR</TableHead>
                  <TableHead>Status</TableHead>
                  <TableHead>Findings</TableHead>
                  <TableHead>Created</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {reviews.map((review) => (
                  <TableRow key={review.id}>
                    <TableCell className="font-mono text-xs">{review.repo}</TableCell>
                    <TableCell>
                      <a
                        href={`https://github.com/${review.repo}/pull/${review.prNumber}`}
                        target="_blank"
                        rel="noreferrer"
                        className="inline-flex items-center gap-1 underline decoration-border underline-offset-4 transition-colors hover:text-foreground"
                      >
                        #{review.prNumber}
                        <ExternalLink className="size-3" aria-hidden />
                      </a>
                    </TableCell>
                    <TableCell>
                      <Badge variant="secondary">{review.status}</Badge>
                    </TableCell>
                    <TableCell>
                      <FindingCounts review={review} />
                    </TableCell>
                    <TableCell className="font-mono text-xs tabular-nums text-muted-foreground">
                      <CreatedAt review={review} now={now} />
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>
        )}
      </div>
    </div>
  );
}
