import { useMemo, useRef, useState } from "react";
import { Link, useParams } from "@tanstack/react-router";
import type { ImperativePanelHandle } from "react-resizable-panels";
import {
  ArrowLeft,
  ChevronDown,
  ChevronRight,
  ExternalLink,
  GitPullRequestArrow,
  Loader2,
  RotateCcw,
  ScanSearch,
  ShieldCheck,
} from "lucide-react";

import type { Review } from "../../gen/engram/app/v1/review_pb";
import { useRetryReview, useReview, useReviews } from "../../hooks/useReviews";
import { useNow } from "../../hooks/useNow";
import { useIsMobile } from "../../hooks/use-mobile";
import { errorMessage } from "../../lib/errors";
import { relativeTime } from "../sessions/session-format";
import { Text } from "@/components/ui/text";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { Sheet, SheetContent, SheetTitle } from "@/components/ui/sheet";
import { ResizableHandle, ResizablePanel, ResizablePanelGroup } from "@/components/ui/resizable";
import { cn } from "@/lib/utils";
import { ReviewGlyph } from "./ReviewGlyph";
import { ReviewProgress } from "./ReviewProgress";
import { FindingsLedger } from "./FindingsLedger";
import {
  ReviewTranscriptPane,
  ROLE_LABEL,
  roleSession,
  type WorkerRole,
} from "./ReviewTranscriptPane";
import { groupByPr, groupOf, supersededBy, type PrGroup } from "./review-groups";
import { judgeFindings } from "./review-findings";
import {
  diffSummary,
  isActive,
  postedReviewUrl,
  prTitleOf,
  prUrl,
  reviewCreatedAt,
  shortSha,
} from "./review-format";

/**
 * One review pass, in full. Addressed by PASS id rather than by PR: the hidden
 * marker in the summary comment engrams posts names the exact pass, so the PR
 * can deep-link straight to the dossier that produced a comment.
 *
 * Reading order is deliberate — identity, then the pass's own conclusion, then
 * the findings, then how it got there. A reader who stops after the verdict band
 * still leaves knowing whether to trust the output.
 */
export function ReviewDossier() {
  const { id } = useParams({ from: "/_app/reviews/$id" });
  const list = useReviews();
  const groups = useMemo(() => groupByPr(list.data?.reviews ?? []), [list.data?.reviews]);
  const group = groupOf(groups, id);

  // The list is already warm from the rail, so the pass's identity renders
  // immediately while the detail (findings, verdicts, log) loads.
  const listed = group?.passes.find((p) => p.id === id);
  const active = listed ? isActive(listed.status) : true;
  const detail = useReview(id, { active });
  const review = detail.data?.review ?? listed;

  if (detail.isPending && !review) {
    return (
      <div className="flex-1 space-y-4 overflow-auto p-4 md:p-6">
        <Skeleton className="h-7 w-80" />
        <Skeleton className="h-4 w-56" />
        <Skeleton className="h-32 w-full" />
      </div>
    );
  }

  if (!review) {
    return (
      <div className="flex-1 overflow-auto p-4 md:p-6">
        <BackToLedger />
        <div role="alert" className="mt-4 rounded-lg border border-dashed p-8 text-center">
          <p className="text-sm text-destructive">
            {detail.error
              ? `Couldn’t load this review. ${errorMessage(detail.error)}`
              : "This review no longer exists."}
          </p>
        </div>
      </div>
    );
  }

  const judged = judgeFindings(detail.data?.findings ?? [], detail.data?.verdicts ?? []);
  const newer = group ? supersededBy(group, review.id) : undefined;

  return (
    <DossierPanes review={review} newer={newer} group={group} detail={detail} judged={judged} />
  );
}

/**
 * The dossier beside an optional worker transcript. Geometry copied from the
 * session detail page, which already proves it: a collapsible resizable pane on
 * desktop with an edge rail to reopen it, and a full-width sheet on a phone.
 */
function DossierPanes({
  review,
  newer,
  group,
  detail,
  judged,
}: {
  review: Review;
  newer: Review | undefined;
  group: PrGroup | undefined;
  detail: ReturnType<typeof useReview>;
  judged: ReturnType<typeof judgeFindings>;
}) {
  const isMobile = useIsMobile();
  const [role, setRole] = useState<WorkerRole | null>(null);
  const paneRef = useRef<ImperativePanelHandle>(null);

  // One entry point for both geometries: pick the role, then reveal the pane.
  const openRole = (next: WorkerRole) => {
    setRole(next);
    paneRef.current?.expand();
  };
  const closePane = () => {
    setRole(null);
    paneRef.current?.collapse();
  };

  const body = (
    <div className="min-h-0 flex-1 overflow-auto">
      <div className="mx-auto flex max-w-4xl flex-col gap-6 p-4 md:p-6">
        <BackToLedger />
        <PrHeader review={review} />
        {newer && <SupersededNotice newer={newer} />}
        <ReviewProgress
          review={review}
          events={detail.data?.events ?? []}
          judged={judged}
          loading={detail.isPending}
          onOpenRole={openRole}
        />
        <FindingsLedger review={review} judged={judged} loading={detail.isPending} />
        {group && group.passes.length > 1 && (
          <PassHistory passes={group.passes} currentId={review.id} />
        )}
      </div>
    </div>
  );

  const openable = (["finder", "verifier"] as const).filter((r) => Boolean(roleSession(review, r)));

  if (isMobile) {
    return (
      <div className="flex min-h-0 flex-1 flex-col overflow-hidden">
        {body}
        <Sheet open={role !== null} onOpenChange={(open) => !open && setRole(null)}>
          <SheetContent side="right" showCloseButton={false} className="w-full gap-0 p-0">
            <SheetTitle className="sr-only">Worker session transcript</SheetTitle>
            {role && (
              <ReviewTranscriptPane
                review={review}
                role={role}
                onChangeRole={setRole}
                onClose={() => setRole(null)}
              />
            )}
          </SheetContent>
        </Sheet>
      </div>
    );
  }

  return (
    <div className="flex min-h-0 flex-1 overflow-hidden">
      <ResizablePanelGroup direction="horizontal" className="min-h-0 flex-1">
        <ResizablePanel id="dossier" order={1} minSize={35} defaultSize={100} className="min-w-0">
          {body}
        </ResizablePanel>
        <ResizableHandle className={role ? "" : "hidden"} />
        <ResizablePanel
          id="transcript"
          order={2}
          ref={paneRef}
          collapsible
          collapsedSize={0}
          minSize={28}
          defaultSize={0}
          onCollapse={() => setRole(null)}
          className="min-w-0 overflow-hidden"
        >
          {role && (
            <ReviewTranscriptPane
              review={review}
              role={role}
              onChangeRole={setRole}
              onClose={closePane}
            />
          )}
        </ResizablePanel>
      </ResizablePanelGroup>

      {/* Collapsed edge rail — the reopen affordance, one glyph per worker whose
          session actually ran. Hidden entirely when no phase ever started. */}
      {!role && openable.length > 0 && (
        <div
          className="flex w-9 shrink-0 flex-col items-center gap-1 border-l bg-background py-2"
          aria-label="Open a worker transcript"
        >
          {openable.map((r) => (
            <Button
              key={r}
              variant="ghost"
              size="icon"
              className="size-7 text-muted-foreground hover:text-foreground"
              title={`Open the ${ROLE_LABEL[r].toLowerCase()} transcript`}
              aria-label={`Open the ${ROLE_LABEL[r].toLowerCase()} transcript`}
              onClick={() => openRole(r)}
            >
              {r === "finder" ? <ScanSearch /> : <ShieldCheck />}
            </Button>
          ))}
        </div>
      )}
    </div>
  );
}

function BackToLedger() {
  return (
    <Link
      to="/reviews"
      className="inline-flex w-fit items-center gap-1.5 text-sm text-muted-foreground underline decoration-border underline-offset-4 transition-colors hover:text-foreground"
    >
      <ArrowLeft className="size-3.5" aria-hidden />
      All reviews
    </Link>
  );
}

/**
 * The PR's identity. The title is what a developer actually remembers, so it
 * leads — but it is captured at pass start and absent on every review recorded
 * before that landed, so the coordinate carries the heading when there is no
 * title rather than a placeholder standing in for one.
 */
function PrHeader({ review }: { review: Review }) {
  const title = prTitleOf(review);
  const diff = diffSummary(review);
  const retry = useRetryReview();
  const reviewUrl = postedReviewUrl(review);

  return (
    <header className="relative flex flex-col gap-3 border-b pb-4">
      <span aria-hidden className="absolute -bottom-px left-0 h-0.5 w-10 bg-primary" />
      <div className="flex flex-wrap items-baseline justify-between gap-x-6 gap-y-2">
        <div className="flex min-w-0 flex-col gap-1">
          <Text
            as="a"
            variant="label"
            tone="muted"
            href={prUrl(review)}
            target="_blank"
            rel="noreferrer"
            className="inline-flex w-fit items-center gap-1.5 transition-colors hover:text-foreground"
          >
            <GitPullRequestArrow className="size-3" aria-hidden />
            {review.repo} #{review.prNumber}
            <ExternalLink className="size-2.5" aria-hidden />
          </Text>
          <Text variant={title ? "display" : "displayMono"} className="text-2xl">
            {title ?? `Pull request #${review.prNumber}`}
          </Text>
        </div>
        <div className="flex shrink-0 items-center gap-2">
          {reviewUrl && (
            <Button variant="outline" size="sm" asChild>
              <a href={reviewUrl} target="_blank" rel="noreferrer">
                <ExternalLink className="size-3.5" aria-hidden />
                Review on GitHub
              </a>
            </Button>
          )}
          {!isActive(review.status) && (
            <Button
              variant="outline"
              size="sm"
              disabled={retry.isPending}
              onClick={() => retry.mutate({ id: review.id })}
            >
              {retry.isPending ? (
                <Loader2 className="size-3.5 animate-spin" aria-hidden />
              ) : (
                <RotateCcw className="size-3.5" aria-hidden />
              )}
              Re-run
            </Button>
          )}
        </div>
      </div>

      {retry.isError && (
        <p role="alert" className="text-xs text-destructive">
          Couldn’t re-run this review. {errorMessage(retry.error)}
        </p>
      )}

      {/* Facts about the PR as it was when this pass ran. Every one of these is
          nullable — a review recorded before capture landed shows only its
          coordinates — so the row is assembled from what exists. */}
      <dl className="flex flex-wrap items-baseline gap-x-5 gap-y-1.5 text-xs text-muted-foreground">
        {review.prAuthor && (
          <Fact label="Author">
            <span className="font-mono">{review.prAuthor}</span>
          </Fact>
        )}
        {review.baseBranch && review.headBranch && (
          <Fact label="Branch">
            <span className="font-mono">
              {review.baseBranch} ← {review.headBranch}
            </span>
          </Fact>
        )}
        {diff && <Fact label="Size">{diff}</Fact>}
        {review.prState && (
          <Fact label="PR was">
            {/* "was", not "is": this is a snapshot from when the pass started and
                is never refreshed, so a PR merged since still reads open. */}
            {review.prState}
          </Fact>
        )}
        <Fact label="Reviewed at">
          <span className="font-mono" title={review.headSha}>
            {shortSha(review.headSha)}
          </span>
        </Fact>
        <Fact label="Triggered by">{review.trigger}</Fact>
      </dl>
    </header>
  );
}

function Fact({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-baseline gap-1.5">
      <dt className="text-muted-foreground/70">{label}</dt>
      <dd className="text-foreground">{children}</dd>
    </div>
  );
}

/**
 * A newer pass over the same PR exists, so this one is history. Derived from the
 * grouping rather than a stored flag — which is why the `superseded` status is
 * never actually written.
 */
function SupersededNotice({ newer }: { newer: Review }) {
  return (
    <p className="flex flex-wrap items-center gap-x-2 gap-y-1 rounded-lg border border-dashed px-3 py-2 text-sm text-muted-foreground">
      A newer pass has run over this pull request.
      <Link
        to="/reviews/$id"
        params={{ id: newer.id }}
        className="inline-flex items-center gap-1 text-foreground underline decoration-border underline-offset-4"
      >
        Open the current pass
        <ChevronRight className="size-3.5" aria-hidden />
      </Link>
    </p>
  );
}

/** Every pass over this PR, so a reader can see the pass this one replaced. */
function PassHistory({ passes, currentId }: { passes: Review[]; currentId: string }) {
  const [open, setOpen] = useState(false);
  const now = useNow();
  const Chevron = open ? ChevronDown : ChevronRight;

  return (
    <section>
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        className="flex w-full items-center gap-1.5 border-b pb-2 text-left transition-colors hover:text-foreground"
      >
        <Chevron className="size-3.5 text-muted-foreground" aria-hidden />
        <Text variant="label" tone="muted">
          Pass history
        </Text>
        <span className="ml-auto font-mono text-xs tabular-nums text-muted-foreground">
          {passes.length}
        </span>
      </button>
      {open && (
        <ol className="mt-2 space-y-1">
          {passes.map((pass) => {
            const at = reviewCreatedAt(pass);
            const isCurrent = pass.id === currentId;
            return (
              <li key={pass.id}>
                <Link
                  to="/reviews/$id"
                  params={{ id: pass.id }}
                  aria-current={isCurrent ? "page" : undefined}
                  className={cn(
                    "flex items-baseline gap-3 rounded-md px-2 py-1.5 text-sm transition-colors",
                    isCurrent ? "bg-accent" : "hover:bg-accent/60",
                  )}
                >
                  <ReviewGlyph status={pass.status} beat={false} className="text-[0.7rem]" />
                  <span className="font-mono text-xs" title={pass.headSha}>
                    {shortSha(pass.headSha)}
                  </span>
                  <span className="text-muted-foreground">{pass.trigger}</span>
                  <span className="ml-auto font-mono text-xs tabular-nums text-muted-foreground">
                    {at ? `${relativeTime(at.toISOString(), now)} ago` : "—"}
                  </span>
                </Link>
              </li>
            );
          })}
        </ol>
      )}
    </section>
  );
}
