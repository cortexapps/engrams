import { useEffect, useState } from "react";
import { LockKeyhole } from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Text } from "@/components/ui/text";
import {
  specPublishRefusal,
  usePublishSpec,
  useSpecPublish,
  type SpecPublishQuestion,
  type SpecPublishRecord,
  type SpecPublishRefusalReason,
  type SpecPublishStatus,
} from "@/hooks/useSpecPublish";
import { useSpecRail } from "@/hooks/useSpecRead";

export function SpecPublishConfirm({
  specId,
  viewerIsOwner,
}: {
  specId: string;
  viewerIsOwner: boolean;
}) {
  const status = useSpecPublish(specId);
  const rail = useSpecRail(specId);
  const publish = usePublishSpec(specId);
  const [open, setOpen] = useState(false);
  const [acknowledged, setAcknowledged] = useState(false);
  const [refusalStatus, setRefusalStatus] = useState<SpecPublishStatus | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (open) return;
    setAcknowledged(false);
    setRefusalStatus(null);
    setError(null);
  }, [open]);

  if (!viewerIsOwner || !status.data) return null;

  const current = refusalStatus ?? status.data;
  const recorded = current.publish;
  const running =
    recorded !== null && recorded.state !== "complete" && recorded.state !== "blocked";
  const finished = current.phase === "published" || recorded?.state === "complete";
  if (finished) return null;

  const questions = current.openQuestions;
  const staleAttempt =
    recorded?.state === "blocked"
      ? "The questions changed before publishing started. Review them and decide again."
      : null;

  const requestPublish = () => {
    if (questions.length > 0 && !acknowledged) {
      setError(
        `Acknowledge the ${questions.length} open ${questions.length === 1 ? "question" : "questions"} before you publish.`,
      );
      return;
    }

    setError(null);
    publish.mutate(
      { acknowledgeOpenQuestions: questions.length > 0 },
      {
        onSuccess: () => setOpen(false),
        onError: (cause) => {
          const refusal = specPublishRefusal(cause);
          if (refusal?.status) setRefusalStatus(refusal.status);
          if (refusal?.reason === "acknowledgment_required") setAcknowledged(false);
          setError(publishErrorMessage(refusal?.reason));
        },
      },
    );
  };

  return (
    <>
      <Button variant="outline" size="sm" onClick={() => setOpen(true)}>
        {running ? "Publishing…" : "Publish"}
      </Button>
      <Dialog open={open} onOpenChange={setOpen}>
        {/* The dialog itself never grows past the viewport: with enough open
            questions the buttons fell below the fold, with the page behind
            scroll-locked — publish was physically unclickable. */}
        <DialogContent className="max-h-[85svh] overflow-y-auto">
          {running ? (
            <PublishingFace publish={recorded} onClose={() => setOpen(false)} />
          ) : (
            <ConfirmFace
              questions={questions}
              completeness={rail.data?.completeness ?? null}
              acknowledged={acknowledged}
              onAcknowledgedChange={(value) => {
                setAcknowledged(value);
                setError(null);
              }}
              pending={publish.isPending}
              error={error ?? staleAttempt}
              onCancel={() => setOpen(false)}
              onPublish={requestPublish}
            />
          )}
        </DialogContent>
      </Dialog>
    </>
  );
}

function ConfirmFace({
  questions,
  completeness,
  acknowledged,
  onAcknowledgedChange,
  pending,
  error,
  onCancel,
  onPublish,
}: {
  questions: SpecPublishQuestion[];
  completeness: { complete: number; total: number } | null;
  acknowledged: boolean;
  onAcknowledgedChange: (value: boolean) => void;
  pending: boolean;
  error: string | null;
  onCancel: () => void;
  onPublish: () => void;
}) {
  const count = questions.length;
  const questionLabel = `${count} open ${count === 1 ? "question" : "questions"}`;
  const unsettled = completeness ? completeness.total - completeness.complete : 0;

  return (
    <>
      <DialogHeader>
        <Text as="span" variant="label" tone="muted">
          Publish spec
        </Text>
        <DialogTitle>
          {count === 0 ? "Publish this spec?" : `Publish with ${questionLabel}?`}
        </DialogTitle>
        <DialogDescription>
          Publishing is irreversible. It is a one-way action: drafting ends, and this spec becomes
          read-only.
        </DialogDescription>
      </DialogHeader>

      {completeness ? (
        <Text {...(unsettled > 0 ? { role: "status" } : { tone: "muted" as const })}>
          {completeness.complete} of {completeness.total} sections settled.
          {unsettled > 0
            ? ` Publishing pins the other ${unsettled} ${unsettled === 1 ? "section" : "sections"} as ${unsettled === 1 ? "it stands" : "they stand"}.`
            : ""}
        </Text>
      ) : null}

      {count > 0 ? (
        <>
          <Text tone="muted">
            {questionLabel} will remain unresolved. They do not block publication.
          </Text>
          <ul
            aria-label="Open questions"
            className="grid max-h-[38svh] gap-3 overflow-y-auto rounded-lg border bg-muted/30 p-4"
          >
            {questions.map((question) => (
              <li key={question.id} className="grid gap-1">
                <Text>{question.text}</Text>
                <Text variant="code" tone="muted" className="text-xs">
                  §{question.sectionTitle}
                </Text>
              </li>
            ))}
          </ul>
          <label className="flex cursor-pointer items-start gap-3 rounded-lg border p-3">
            <input
              type="checkbox"
              className="mt-0.5 size-4 accent-primary"
              checked={acknowledged}
              onChange={(event) => onAcknowledgedChange(event.target.checked)}
            />
            <Text as="span">
              I acknowledge that {questionLabel} will remain unresolved when I publish.
            </Text>
          </label>
        </>
      ) : (
        <Text tone="muted">This spec has 0 open questions.</Text>
      )}

      {error ? (
        <Text role="alert" tone="destructive">
          {error}
        </Text>
      ) : null}

      <div className="flex items-center gap-2 rounded-md bg-secondary px-3 py-2">
        <LockKeyhole aria-hidden="true" className="size-4 shrink-0" />
        <Text variant="code" tone="muted" className="text-xs">
          One published version · no return to drafting
        </Text>
      </div>

      <DialogFooter>
        <Button type="button" variant="ghost" onClick={onCancel}>
          Keep drafting
        </Button>
        <Button type="button" disabled={pending} onClick={onPublish}>
          {pending ? "Publishing…" : "Publish"}
        </Button>
      </DialogFooter>
    </>
  );
}

function PublishingFace({ publish, onClose }: { publish: SpecPublishRecord; onClose: () => void }) {
  return (
    <>
      <DialogHeader>
        <Text as="span" variant="label" tone="muted">
          Publish spec
        </Text>
        <DialogTitle>Publishing this spec</DialogTitle>
        <DialogDescription>
          The request started at {formatTime(publish.requestedAt)}. Publishing continues if you
          close this window.
        </DialogDescription>
      </DialogHeader>
      {publish.acknowledgedQuestionCount > 0 ? (
        <Text tone="muted">
          {publish.acknowledgedQuestionCount} open{" "}
          {publish.acknowledgedQuestionCount === 1 ? "question was" : "questions were"}{" "}
          acknowledged.
        </Text>
      ) : null}
      {publish.lastError ? (
        <Text role="alert" tone="destructive">
          The last publish step did not finish. It retries automatically.
        </Text>
      ) : null}
      <DialogFooter>
        <Button type="button" variant="outline" onClick={onClose}>
          Done
        </Button>
      </DialogFooter>
    </>
  );
}

function publishErrorMessage(reason: SpecPublishRefusalReason | undefined) {
  switch (reason) {
    case "ideation":
      return "Start drafting before you publish this spec.";
    case "acknowledgment_required":
      return "Review and acknowledge the current open questions before you publish.";
    case "not_owner":
      return "Only the spec owner can publish this spec.";
    case "already_published":
      return "This spec is already published.";
    case "no_session":
      return "This spec has no drafting session, so it cannot be published.";
    default:
      return "The spec did not publish. Try again.";
  }
}

function formatTime(iso: string): string {
  const parsed = new Date(iso);
  if (Number.isNaN(parsed.getTime())) return iso;
  return parsed.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
}
