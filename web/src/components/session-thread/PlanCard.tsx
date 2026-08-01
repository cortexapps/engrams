import {
  CheckIcon,
  ChevronDownIcon,
  ChevronRightIcon,
  Loader2Icon,
  MapIcon,
  Undo2Icon,
} from "lucide-react";
import { useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Text } from "@/components/ui/text";
import { Textarea } from "@/components/ui/textarea";
import { Markdown } from "@/components/Markdown";
import { isSubmitKey, useEnterToSend } from "@/hooks/useEnterToSend";
import { cn } from "@/lib/utils";
import { hms } from "../transcriptFmt";
import type { SystemMarker } from "./buildMessages";
import { useQuestionActions } from "./question-actions";

// ADR 0107: the interactive plan-review card. The plan is a DOCUMENT (median
// ~13KB markdown) rendered in an internally scrolled region; the review is a
// DECISION — the approve/reject bar sits below the scroll region so it is
// always visible. Resolved cards collapse to a one-line receipt so
// reject → revise cycles read as quiet history, not stacked documents.

type PlanMarker = Extract<SystemMarker, { kind: "plan" }>;

/** First markdown heading (or first non-empty line), for the receipt. */
function planTitle(plan: string): string {
  for (const raw of plan.split("\n")) {
    const line = raw.trim();
    if (!line) continue;
    return line.replace(/^#+\s*/, "");
  }
  return "plan";
}

export function PlanCard({ marker }: { marker: PlanMarker }) {
  const { completeTool, answeredToolCallIds, sendBlocked } = useQuestionActions();
  // Feedback is a message to the agent, so it obeys the same send chord the
  // composer does (the user's Enter-to-send preference), not a hardcoded ⌘↵.
  const [enterToSend] = useEnterToSend();
  const [feedbackOpen, setFeedbackOpen] = useState(false);
  const [feedback, setFeedback] = useState("");
  // Optimistic verdict while the RPC + wake round-trips (the VM may take
  // seconds to resume); replaced by the authoritative resolution event.
  const [optimistic, setOptimistic] = useState<"approve" | "reject" | null>(null);

  const confirmed = marker.resolution != null;
  const pending = !confirmed && answeredToolCallIds.has(marker.toolCallId);
  const decided = confirmed || pending;
  const approved = confirmed ? marker.resolution!.approved : optimistic === "approve";

  const decide = (decision: "approve" | "reject") => {
    if (decided || sendBlocked) return;
    setOptimistic(decision);
    completeTool(marker.toolCallId, {
      decision,
      ...(decision === "reject" && feedback.trim() ? { feedback: feedback.trim() } : {}),
    });
  };

  // Resolved (or optimistically resolved): the one-line receipt, expandable
  // back into the (read-only) document.
  if (decided) {
    return <PlanReceipt marker={marker} approved={approved} pending={pending} />;
  }

  const live = !sendBlocked;
  return (
    <Card className={cn("py-0", live && "border-primary/40")}>
      <CardContent className="flex flex-col gap-0 p-0">
        <div className="flex items-center gap-2 border-b px-4 py-3 text-xs">
          <MapIcon className={cn("size-3.5", live ? "text-primary" : "text-muted-foreground")} />
          <Text as="span" variant="label" className={cn(live && "text-primary")}>
            {live ? "plan ready — awaiting your review" : "plan proposed"}
          </Text>
          {marker.revision > 1 && (
            <Badge variant="outline" className="text-[10px]">
              rev {marker.revision}
            </Badge>
          )}
          <span className="ml-auto font-mono tabular-nums text-muted-foreground">
            {hms(marker.at)}
          </span>
        </div>
        <div className="max-h-[32rem] overflow-auto overscroll-contain border-b px-4 py-3">
          <div className="max-w-[70ch] text-sm leading-relaxed">
            <Markdown text={marker.plan} />
          </div>
        </div>
        {feedbackOpen ? (
          <div className="flex flex-col gap-2 p-3">
            <Textarea
              autoFocus
              rows={2}
              value={feedback}
              placeholder="What should change?"
              onChange={(e) => setFeedback(e.target.value)}
              onKeyDown={(e) => {
                if (isSubmitKey(e, enterToSend)) {
                  e.preventDefault();
                  decide("reject");
                } else if (e.key === "Escape") {
                  e.preventDefault();
                  e.stopPropagation();
                  setFeedbackOpen(false);
                }
              }}
            />
            <div className="flex flex-wrap items-center gap-2">
              <Button size="sm" disabled={sendBlocked} onClick={() => decide("reject")}>
                Send feedback
              </Button>
              <Button size="sm" variant="ghost" onClick={() => setFeedbackOpen(false)}>
                Cancel
              </Button>
              <span className="ml-auto text-xs italic text-muted-foreground">
                {enterToSend ? "↵ to send" : "⌘↵ to send"}
              </span>
            </div>
          </div>
        ) : (
          <div className="flex flex-wrap items-center gap-2 p-3">
            <Button size="sm" disabled={sendBlocked} onClick={() => decide("approve")}>
              Approve &amp; build
            </Button>
            <Button
              size="sm"
              variant="outline"
              disabled={sendBlocked}
              onClick={() => setFeedbackOpen(true)}
            >
              Request changes
            </Button>
            {sendBlocked && (
              <span className="text-xs italic text-muted-foreground">
                Session is no longer live — can&apos;t review.
              </span>
            )}
          </div>
        )}
      </CardContent>
    </Card>
  );
}

function PlanReceipt({
  marker,
  approved,
  pending,
}: {
  marker: PlanMarker;
  approved: boolean;
  pending: boolean;
}) {
  const [open, setOpen] = useState(false);
  const feedback = marker.resolution?.feedback ?? null;
  const Chevron = open ? ChevronDownIcon : ChevronRightIcon;
  return (
    <Card className={cn("py-0", pending && "opacity-60")}>
      <CardContent className="flex flex-col gap-0 p-0">
        <button
          type="button"
          className="flex w-full items-center gap-2 px-4 py-3 text-left text-xs"
          onClick={() => setOpen((o) => !o)}
        >
          {pending ? (
            <Loader2Icon className="size-3.5 animate-spin text-muted-foreground" />
          ) : approved ? (
            <CheckIcon className="size-3.5 text-primary" />
          ) : (
            <Undo2Icon className="size-3.5 text-muted-foreground" />
          )}
          <Text as="span" variant="label">
            {pending
              ? approved
                ? "starting build…"
                : "sending feedback…"
              : approved
                ? "plan approved"
                : "changes requested"}
          </Text>
          <span className="truncate text-muted-foreground">{planTitle(marker.plan)}</span>
          {marker.revision > 1 && (
            <Badge variant="outline" className="text-[10px]">
              rev {marker.revision}
            </Badge>
          )}
          <span className="ml-auto flex items-center gap-2 font-mono tabular-nums text-muted-foreground">
            {hms(marker.at)}
            <Chevron className="size-3.5" />
          </span>
        </button>
        {feedback && !approved && (
          <div className="border-t px-4 py-2 text-xs italic text-muted-foreground">
            “{feedback}”
          </div>
        )}
        {open && (
          <div className="max-h-[32rem] overflow-auto overscroll-contain border-t px-4 py-3">
            <div className="max-w-[70ch] text-sm leading-relaxed">
              <Markdown text={marker.plan} />
            </div>
          </div>
        )}
      </CardContent>
    </Card>
  );
}
