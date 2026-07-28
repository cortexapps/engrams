import { useEffect, useMemo, useState } from "react";
import { Link, useNavigate, useParams } from "@tanstack/react-router";
import { subject } from "@casl/ability";
import {
  ArrowLeft,
  Check,
  ChevronDown,
  ExternalLink,
  Loader2,
  MessagesSquare,
  RotateCcw,
} from "lucide-react";

import type { Review } from "../../gen/engram/app/v1/review_pb";
import { useRetryReview, useReview, useReviews } from "../../hooks/useReviews";
import { useNow } from "../../hooks/useNow";
import { useAbility } from "../../auth/AuthProvider";
import type { SessionSubject } from "../../lib/ability";
import { errorMessage } from "../../lib/errors";
import { relativeTime } from "../sessions/session-format";
import { Text } from "@/components/ui/text";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { Sheet, SheetContent, SheetTitle } from "@/components/ui/sheet";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { cn } from "@/lib/utils";
import { ReviewStage } from "./ReviewGlyph";
import { PassState } from "./PassState";
import { FindingsLedger } from "./FindingsLedger";
import { ReviewTranscriptPane, roleSession, type WorkerRole } from "./ReviewTranscriptPane";
import { groupByPr, groupOf } from "./review-groups";
import { judgeFindings, keptRatio, type JudgedFinding } from "./review-findings";
import {
  diffSummary,
  failureReason,
  isActive,
  isHumanTrigger,
  passDuration,
  postedReviewUrl,
  prTitleOf,
  prUrl,
  reviewCreatedAt,
  shortSha,
  triggerOf,
} from "./review-format";

/**
 * One reviewed pull request, with the addressed pass selected.
 *
 * The route carries a PASS id — the hidden marker in the summary comment engrams
 * posts names the exact pass, so a PR can deep-link to the pass that produced a
 * comment. What it lands on is the PR, and the findings are the content: identity
 * is one line, the pass is one line, and everything about HOW a pass ran is a
 * click behind that line. Nothing sits below the findings.
 */
export function ReviewDossier() {
  const { id } = useParams({ from: "/_app/reviews/$id" });
  const list = useReviews();
  const groups = useMemo(() => groupByPr(list.data?.reviews ?? []), [list.data?.reviews]);
  const group = groupOf(groups, id);

  // The list is already warm from the rail, so the PR's identity and its pass
  // switcher render immediately while the selected pass's detail loads.
  const listed = group?.passes.find((p) => p.id === id);
  const active = listed ? isActive(listed) : true;
  const detail = useReview(id, { active });
  const review = detail.data?.review ?? listed;

  const [role, setRole] = useState<WorkerRole | null>(null);

  if (detail.isPending && !review) {
    return (
      <Frame>
        <div className="flex flex-col gap-3">
          <Skeleton className="h-8 w-80" />
          <Skeleton className="h-4 w-56" />
        </div>
        <Skeleton className="h-24 w-full" />
      </Frame>
    );
  }

  if (!review) {
    return (
      <Frame>
        <div className="flex flex-col gap-4">
          <BackToLedger />
          <p role="alert" className="text-sm text-destructive">
            {detail.error
              ? `Couldn’t load this review. ${errorMessage(detail.error)}`
              : "This review no longer exists."}
          </p>
        </div>
      </Frame>
    );
  }

  // Passes newest first. Without the list (a direct load that hasn't landed, or a
  // pass the list doesn't carry) the selected pass stands alone.
  const passes = group?.passes ?? [review];
  // The review is an input to outcome derivation, not just a subject of it: a
  // finding's reason for not posting depends on whether the posting gate ran.
  const judged = judgeFindings(detail.data?.findings ?? [], detail.data?.verdicts ?? [], review);

  return (
    <>
      <Frame>
        <div className="flex flex-col gap-4">
          <BackToLedger />
          <PrHeader pass={review} latestPassId={passes[0]?.id ?? review.id} />
        </div>

        <div className="flex flex-col gap-5">
          <PassLine
            passes={passes}
            pass={review}
            judged={judged}
            detail={detail}
            onOpenSessions={setRole}
          />
          <FindingsLedger review={review} judged={judged} loading={detail.isPending} />
        </div>
      </Frame>

      {/* Worker transcripts arrive as a sheet over the dossier rather than a
          resizable column: they explain how a pass reached its conclusion, they
          are not a second thing to read alongside it. */}
      <Sheet open={role !== null} onOpenChange={(open) => !open && setRole(null)}>
        <SheetContent
          side="right"
          showCloseButton={false}
          className="w-full gap-0 p-0 sm:max-w-xl lg:max-w-2xl"
        >
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
    </>
  );
}

/** The one scroll container for the route, and the measure everything sits in. */
function Frame({ children }: { children: React.ReactNode }) {
  return (
    <div className="min-h-0 flex-1 overflow-y-auto">
      <div className="mx-auto flex max-w-4xl flex-col gap-7 p-4 md:p-6 lg:py-8">{children}</div>
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
 * The pull request, named once. The title is what a developer remembers, so it is
 * the heading — but it is captured at pass start and absent on every review
 * recorded before that landed, so the coordinate carries the heading when there
 * is no title rather than a placeholder standing in for one.
 *
 * One GitHub affordance, not two: it lands on the review engrams posted when there
 * is one and on the PR itself otherwise. Both are the same page over there, so a
 * second link would only make a reader choose between them.
 */
function PrHeader({
  pass,
  latestPassId,
}: {
  pass: Review;
  /** The newest pass known right now, so a re-run can follow the one it starts. */
  latestPassId: string;
}) {
  // The PR's name rides on the PR's own record now (ADR 0100 d11), so this pass
  // carries it whether or not this pass is the one that learned it — no need to
  // borrow identity from the newest sibling.
  const title = prTitleOf(pass);
  const retry = useRetryReview();
  const navigate = useNavigate();
  // One GitHub affordance, but never a label that lies about where it lands.
  const reviewUrl = postedReviewUrl(pass);

  // A re-run cannot tell you the new pass's id: the RPC starts durable ingress
  // and returns only its workflow id. Ingress writes the row shortly afterward,
  // so wait for the list to show a pass newer than the one we started from, then
  // follow it.
  const [awaiting, setAwaiting] = useState<string | null>(null);
  useEffect(() => {
    if (awaiting && latestPassId !== awaiting) {
      setAwaiting(null);
      void navigate({ to: "/reviews/$id", params: { id: latestPassId } });
    }
  }, [awaiting, latestPassId, navigate]);

  return (
    <header className="flex flex-col gap-2.5">
      <div className="flex flex-wrap items-start justify-between gap-x-6 gap-y-3">
        <Text
          as="h1"
          variant={title ? "display" : "displayMono"}
          className="min-w-0 text-2xl md:text-[1.75rem]"
        >
          {title ?? `Pull request #${pass.prNumber}`}
        </Text>
        <div className="flex shrink-0 items-center gap-1">
          <Button variant="ghost" size="sm" asChild>
            <a href={reviewUrl ?? prUrl(pass)} target="_blank" rel="noreferrer">
              <ExternalLink className="size-3.5" aria-hidden />
              {reviewUrl ? "Review on GitHub" : "Pull request on GitHub"}
            </a>
          </Button>
          {!isActive(pass) && (
            <Button
              variant="outline"
              size="sm"
              disabled={retry.isPending || awaiting !== null}
              // A re-run mints a NEW pass, so staying put would leave the reader
              // on history while the thing they asked for runs out of sight.
              onClick={() =>
                retry.mutate({ id: pass.id }, { onSuccess: () => setAwaiting(latestPassId) })
              }
            >
              {retry.isPending || awaiting ? (
                <Loader2 className="size-3.5 animate-spin" aria-hidden />
              ) : (
                <RotateCcw className="size-3.5" aria-hidden />
              )}
              Re-run
            </Button>
          )}
        </div>
      </div>

      <PrMeta review={pass} />

      {retry.isError && (
        <p role="alert" className="text-xs text-destructive">
          Couldn’t re-run this review. {errorMessage(retry.error)}
        </p>
      )}
    </header>
  );
}

/**
 * The PR in one line, assembled from whatever was captured — every field is
 * nullable, and a review recorded before capture landed has only its coordinate.
 * A list of what exists reads faster than a grid of labels over empty values.
 *
 * Two kinds of fact sit here and the distinction is real (ADR 0100 d11): the
 * coordinate, author and state describe the PR and are current; the branches and
 * the diff size describe what THIS pass read and stay frozen at its head.
 */
function PrMeta({ review }: { review: Review }) {
  const facts: Array<[string, React.ReactNode]> = [
    [
      "coordinate",
      <span className="font-mono">
        {review.repo} #{review.prNumber}
      </span>,
    ],
  ];
  if (review.prAuthor) facts.push(["author", <span className="font-mono">{review.prAuthor}</span>]);
  if (review.baseBranch && review.headBranch) {
    facts.push([
      "branch",
      <span className="font-mono">
        {review.baseBranch} ← {review.headBranch}
      </span>,
    ]);
  }
  const diff = diffSummary(review);
  if (diff) facts.push(["size", <span className="tabular-nums">{diff}</span>]);
  if (review.prState) {
    // Plain "merged", not "was merged": the state lives on the PR's own record
    // and is rewritten by every pass, so it is current as of the newest one.
    facts.push(["state", <span>{review.prState}</span>]);
  }

  return (
    <p className="flex flex-wrap items-baseline gap-x-2 gap-y-1 text-xs text-muted-foreground">
      {facts.map(([key, node], i) => (
        <span key={key} className="flex items-baseline gap-2">
          {i > 0 && (
            <span aria-hidden className="select-none text-muted-foreground/40">
              ·
            </span>
          )}
          {node}
        </span>
      ))}
    </p>
  );
}

/**
 * The selected pass, in one line — but a line with a rank, not a run of six
 * equally weighted tokens.
 *
 * Four groups, in order of what a reader needs: **what happened** as the row's
 * headline, **which pass** in a bordered control so its boundary is visible,
 * **when and how long** as a quiet mono readout, and **the two ways in** as a
 * grouped pair on the right. The stage sits outside the switcher on purpose:
 * "Failed" is not part of choosing a pass, and folding it into the trigger was
 * what made the whole strip read as one undifferentiated token stream.
 */
function PassLine({
  passes,
  pass,
  judged,
  detail,
  onOpenSessions,
}: {
  passes: Review[];
  pass: Review;
  judged: JudgedFinding[];
  detail: ReturnType<typeof useReview>;
  onOpenSessions: (role: WorkerRole) => void;
}) {
  const now = useNow();
  const at = reviewCreatedAt(pass);
  const ratio = keptRatio(judged);
  const live = isActive(pass);
  // A reviewer session is owned by nobody — `insertReviewTask` stores
  // `createdByUserId: null` — and session reads are owner-scoped, so only an
  // admin can actually open one. Asking the ability the real question rather
  // than checking a role keeps the button honest if that rule ever changes:
  // offering a control that answers 404 is worse than not offering it.
  const canReadWorkers = useAbility().can(
    "read",
    subject("Session", { createdByUserId: null } satisfies SessionSubject),
  );
  const hasWorker =
    canReadWorkers && (["finder", "verifier"] as const).some((r) => Boolean(roleSession(pass, r)));
  const events = detail.data?.events ?? [];
  const failure = failureReason(pass, events);
  // The canonical entry point is a marker in a comment on a PR that has very
  // likely been pushed to since. Reading history believing it is current is the
  // trust failure this line exists to prevent.
  const newest = passes[0];
  const superseded = newest && newest.id !== pass.id ? newest : undefined;
  const supersededAt = superseded ? reviewCreatedAt(superseded) : undefined;

  // Duration belongs to the readout, not to the activity button's label — a
  // control labelled "3m 13s" is the reason a failed pass's cause was unfindable.
  const duration = passDuration(events);
  const lone = passes.length === 1;

  return (
    <div className="flex flex-col gap-1.5">
      <div className="flex flex-wrap items-center gap-x-4 gap-y-2">
        {/* 1. What it is doing, or what it did — and the log behind it. State and
            activity are one idea at two zoom levels, and a terminal pass's state is
            literally the last line of its own log, so they are one control. */}
        <PassState review={pass} events={events} loading={detail.isPending} />

        {/* 2. Which pass. A bordered control, because the previous version left a
            reader guessing where the clickable thing began and ended. */}
        <PassSwitcher passes={passes} pass={pass} now={now} />

        {/* 3. When and how long — one mono readout, not two competing time facts.
            The SHA joins it when there is no switcher to carry it. */}
        {(lone || duration || at) && (
          <span className="flex items-baseline gap-2 font-mono text-xs tabular-nums text-muted-foreground">
            {lone && <span title={pass.headSha}>{shortSha(pass.headSha)}</span>}
            {duration && <Dot show={lone}>{duration}</Dot>}
            {at && (
              <Dot show={lone || Boolean(duration)}>
                <span title={at.toLocaleString()}>{relativeTime(at.toISOString(), now)} ago</span>
              </Dot>
            )}
          </span>
        )}

        {/* 4. The way into the worker transcripts. */}
        {hasWorker && (
          <Button
            variant="ghost"
            size="sm"
            className="ml-auto shrink-0 text-muted-foreground hover:text-foreground"
            onClick={() => onOpenSessions(defaultRole(pass))}
          >
            <MessagesSquare className="size-3.5" aria-hidden />
            Sessions
          </Button>
        )}
      </div>

      {/* Why it ended badly, in the reading flow rather than inside the activity
          popover behind a control labelled by duration. Without it an operator
          cannot tell an infrastructure failure (re-run helps) from one about their
          own PR (it won't). */}
      {failure && <p className="text-sm">{failure}</p>}

      {/* Stated only when the verifier actually killed something. A pass with
          nothing refuted has no ratio worth reading, and the findings below carry
          their own counts. Deliberately silent about what "stands": an unverified
          finding never posts, so counting it as surviving would overstate. */}
      {!live && ratio.refuted > 0 && (
        <p className="text-sm text-muted-foreground">
          The verifier refuted {ratio.refuted} of {ratio.total}.
        </p>
      )}

      {superseded && (
        <p className="text-sm text-muted-foreground">
          A newer pass ran
          {supersededAt ? ` ${relativeTime(supersededAt.toISOString(), now)} ago` : ""}.{" "}
          <Link
            to="/reviews/$id"
            params={{ id: superseded.id }}
            className="text-foreground underline decoration-border underline-offset-4"
          >
            Open it
          </Link>
        </p>
      )}
    </div>
  );
}

/** A leading interpunct, so a readout can drop segments without stranding one. */
function Dot({ show, children }: { show: boolean; children: React.ReactNode }) {
  return (
    <span className="flex items-baseline gap-2">
      {show && (
        <span aria-hidden className="select-none text-muted-foreground/40">
          ·
        </span>
      )}
      {children}
    </span>
  );
}

/**
 * Which pass you are reading, and the way to any other.
 *
 * A retry and every push mint another pass, so a PR carries a handful. As a menu
 * they cost one line instead of a list the findings would have to sit below — and
 * every pass stays one click away, which is what retires the old "a newer pass has
 * run" notice that made a deep link to an older pass feel like a dead end.
 */
function PassSwitcher({ passes, pass, now }: { passes: Review[]; pass: Review; now: number }) {
  // One pass is not a choice, and a disabled-looking control implying six hidden
  // siblings would be a lie. The readout beside this carries the SHA instead.
  if (passes.length === 1) return null;

  // Passes are held newest-first for display, but NUMBERED oldest-first, the way
  // anyone counts attempts: pass 7 of 7 is the latest, pass 1 was the first try.
  // Ranking by recency read as exactly the opposite of what it meant.
  const ordinal = (item: Review) => passes.length - passes.findIndex((p) => p.id === item.id);

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        {/* A real boundary. As bare text with a chevron somewhere after it, nothing
            said where the control started or that it was a control at all. */}
        <Button variant="outline" size="sm" className="gap-2 font-normal">
          <span className="font-mono text-xs tabular-nums" title={pass.headSha}>
            {shortSha(pass.headSha)}
          </span>
          <span aria-hidden className="text-muted-foreground/40">
            ·
          </span>
          <span className="text-xs text-muted-foreground">
            pass {ordinal(pass)} of {passes.length}
          </span>
          <ChevronDown className="size-3.5 text-muted-foreground" aria-hidden />
          <span className="sr-only">Choose a pass</span>
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="w-80">
        <DropdownMenuLabel className="text-muted-foreground">
          {passes.length} passes over this pull request
        </DropdownMenuLabel>
        {passes.map((item) => {
          const at = reviewCreatedAt(item);
          const trigger = triggerOf(item.trigger);
          // Automation is the ordinary case, so only a pass a PERSON asked for
          // carries a marker.
          const HumanIcon = isHumanTrigger(item) ? trigger.icon : null;
          const current = item.id === pass.id;

          return (
            <DropdownMenuItem key={item.id} asChild>
              <Link
                to="/reviews/$id"
                params={{ id: item.id }}
                aria-current={current ? "page" : undefined}
                className="flex items-center gap-2"
              >
                <Check className={cn("size-3.5 shrink-0", !current && "invisible")} aria-hidden />
                {current && <span className="sr-only">Currently open —</span>}
                <span className="w-4 shrink-0 font-mono text-[0.7rem] tabular-nums text-muted-foreground">
                  {ordinal(item)}
                </span>
                {/* The word, not just the glyph: seven rows of `✓ abc1234 3d` make
                    choosing a pass an exercise in recalling an alphabet, and leave
                    a screen reader hearing no status at all. */}
                <ReviewStage status={item.status} className="shrink-0 text-xs" beat={false} />
                <span className={cn("font-mono text-xs tabular-nums", current && "font-medium")}>
                  {shortSha(item.headSha)}
                </span>
                {HumanIcon && (
                  <>
                    <HumanIcon className="size-3 shrink-0 text-muted-foreground" aria-hidden />
                    <span className="sr-only">{trigger.label}</span>
                  </>
                )}
                <span className="ml-auto shrink-0 font-mono text-[0.7rem] tabular-nums text-muted-foreground">
                  {at ? relativeTime(at.toISOString(), now) : "—"}
                </span>
              </Link>
            </DropdownMenuItem>
          );
        })}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

/** The role a reader most likely wants: whichever phase is running now, else the
 *  finder, whose transcript is what explains the findings. */
function defaultRole(pass: Review): WorkerRole {
  if (pass.status === "verifying" && pass.verifierSessionId) return "verifier";
  return pass.finderSessionId ? "finder" : "verifier";
}
