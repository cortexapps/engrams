import { CheckIcon, Loader2Icon, MessageCircleQuestionIcon } from "lucide-react";
import { useMemo, useState } from "react";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Text } from "@/components/ui/text";
import { cn } from "@/lib/utils";
import { hms } from "../transcriptFmt";
import type { SystemMarker } from "./buildMessages";
import { useQuestionActions } from "./question-actions";

// ADR 0054: the interactive AskUserQuestion card. While unanswered it's a form
// — one block per question, options as selectable rows (single-select behaves
// like radios, multi-select toggles). On submit it uses the protocol recorded
// on the marker (CompleteToolCall for generic, AnswerQuestion for historical)
// and locks into a read-only receipt; the authoritative resolving SSE event
// shows the same thing.

type QuestionMarker = Extract<SystemMarker, { kind: "user_question" }>;

/** Selections keyed by question text → chosen option labels. */
type Selections = Record<string, string[]>;

export function UserQuestionCard({ marker }: { marker: QuestionMarker }) {
  const { submitAnswer, answeredToolCallIds, sendBlocked } = useQuestionActions();
  const [selections, setSelections] = useState<Selections>({});

  // `confirmed` = the authoritative result/answered event has landed.
  // `pending` = we optimistically submitted but it hasn't round-tripped yet
  // (the VM may take seconds to resume and re-fire). Both show the receipt;
  // pending shows it greyed with a spinner, mirroring the optimistic
  // user-bubble pattern elsewhere.
  const confirmed = marker.answers != null;
  const pending = !confirmed && answeredToolCallIds.has(marker.toolCallId);
  const answered = confirmed || pending;
  // The answers to render in the receipt: authoritative once confirmed, the
  // local picks while still pending, none while unanswered.
  const resolved = confirmed ? marker.answers : pending ? selections : null;

  const toggle = (q: string, label: string, multi: boolean) => {
    setSelections((prev) => {
      const cur = prev[q] ?? [];
      if (multi) {
        return {
          ...prev,
          [q]: cur.includes(label) ? cur.filter((l) => l !== label) : [...cur, label],
        };
      }
      return { ...prev, [q]: cur[0] === label ? [] : [label] };
    });
  };

  // Ready to submit once every question has at least one selection.
  const complete = useMemo(
    () => marker.questions.every((q) => (selections[q.question]?.length ?? 0) > 0),
    [marker.questions, selections],
  );

  const onSubmit = () => {
    if (!complete || answered || sendBlocked) return;
    submitAnswer(marker.via, marker.toolCallId, selections);
  };

  return (
    <Card className={cn("py-0", !answered && "border-primary/40")}>
      <CardContent className={cn("flex flex-col gap-4 p-4", pending && "opacity-60")}>
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <MessageCircleQuestionIcon className="size-3.5 text-primary" />
          <Text as="span" variant="label" tone={answered ? "muted" : "primary"}>
            {confirmed ? "answered" : pending ? "saving…" : "needs your input"}
          </Text>
          {pending && <Loader2Icon className="size-3.5 animate-spin text-muted-foreground" />}
          <span className="ml-auto font-mono tabular-nums">{hms(marker.at)}</span>
        </div>

        {marker.questions.map((q, qi) => {
          const chosen = resolved?.[q.question] ?? [];
          return (
            <div key={qi} className="flex flex-col gap-2">
              <div className="flex flex-col gap-0.5">
                {q.header && (
                  <Text as="span" variant="label" tone="muted">
                    {q.header}
                  </Text>
                )}
                <p className="text-sm font-medium text-foreground">{q.question}</p>
                {!answered && q.multiSelect && (
                  <span className="text-xs text-muted-foreground italic">
                    Select all that apply.
                  </span>
                )}
              </div>

              {answered ? (
                <div className="flex flex-col gap-1">
                  {chosen.length === 0 ? (
                    <span className="text-sm text-muted-foreground italic">— no answer —</span>
                  ) : (
                    chosen.map((label) => (
                      <div key={label} className="flex items-center gap-2 text-sm">
                        <CheckIcon className="size-3.5 shrink-0 text-primary" />
                        <span>{label}</span>
                      </div>
                    ))
                  )}
                </div>
              ) : (
                <div
                  role={q.multiSelect ? "group" : "radiogroup"}
                  aria-label={q.question}
                  className="flex flex-col gap-1.5"
                >
                  {q.options.map((opt) => {
                    const selected = (selections[q.question] ?? []).includes(opt.label);
                    return (
                      <button
                        key={opt.label}
                        type="button"
                        role={q.multiSelect ? "checkbox" : "radio"}
                        aria-checked={selected}
                        onClick={() => toggle(q.question, opt.label, q.multiSelect)}
                        className={cn(
                          "flex items-start gap-2.5 rounded-md border p-2.5 text-left transition-colors",
                          "hover:bg-muted/60 focus-visible:ring-2 focus-visible:ring-ring/40 focus-visible:outline-none",
                          selected ? "border-primary bg-primary/5" : "border-border",
                        )}
                      >
                        <span
                          className={cn(
                            "mt-0.5 flex size-4 shrink-0 items-center justify-center border",
                            q.multiSelect ? "rounded-sm" : "rounded-full",
                            selected
                              ? "border-primary bg-primary text-primary-foreground"
                              : "border-muted-foreground/50",
                          )}
                        >
                          {selected && <CheckIcon className="size-3" />}
                        </span>
                        <span className="flex flex-col gap-0.5">
                          <span className="text-sm font-medium text-foreground">{opt.label}</span>
                          {opt.description && (
                            <span className="text-xs text-muted-foreground">{opt.description}</span>
                          )}
                        </span>
                      </button>
                    );
                  })}
                </div>
              )}
            </div>
          );
        })}

        {!answered && (
          <div className="flex items-center gap-3">
            <Button size="sm" disabled={!complete || sendBlocked} onClick={onSubmit}>
              Submit answer
            </Button>
            {sendBlocked && (
              <span className="text-xs text-muted-foreground italic">
                Session is no longer live — can't answer.
              </span>
            )}
          </div>
        )}
      </CardContent>
    </Card>
  );
}
