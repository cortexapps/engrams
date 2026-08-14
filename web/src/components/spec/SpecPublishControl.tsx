import { CircleDotDashed, Flag, Lock, TriangleAlert } from "lucide-react";
import { useEffect, useState } from "react";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  specPublishRefusal,
  usePublishSpec,
  useSpecPublish,
  type SpecPublishGate,
  type SpecPublishStatus,
} from "@/hooks/useSpecPublish";

import "./spec-publish.css";

export interface SpecPublishControlProps {
  specId: string;
  /** Puts the person in the blocking section, one click from the blocker. */
  onReviewSection: (sectionId: string) => void;
}

/**
 * The publish gate (mock 2k, R34-R38).
 *
 * The button is the readiness signal: it stays quiet while the gate blocks and
 * it fills only when the gate would pass. A primary that is always lit and
 * mostly opens a "not yet" dialog teaches people to ignore the accent colour.
 *
 * The server's own status decides what this renders, so the browser never keeps
 * a second opinion about who may publish or what is left to do.
 */
export function SpecPublishControl({ specId, onReviewSection }: SpecPublishControlProps) {
  const status = useSpecPublish(specId);
  const publish = usePublishSpec(specId);
  const [open, setOpen] = useState(false);
  const [acknowledged, setAcknowledged] = useState(false);
  const [refusal, setRefusal] = useState<SpecPublishStatus | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (open) return;
    setAcknowledged(false);
    setRefusal(null);
    setError(null);
  }, [open]);

  if (!status.data) return null;
  const current = refusal ?? status.data;
  // A publish the pin refused is not running: the spec is still in drafting, and
  // the gate is what the person needs next. Its reason rides along.
  const refused = current.publish?.state === "blocked" ? current.publish.lastError : null;
  const running =
    current.publish !== null &&
    current.publish.state !== "complete" &&
    current.publish.state !== "blocked";
  // A finished publish needs no control: the page header already says
  // published. A running one stays reachable, because its steps can still
  // fail and retry, and hiding that would hide the only honest signal.
  if (!running && !current.canPublish) return null;

  const gate = current.gate;
  const label = running ? "Publishing…" : gate.gapCheckRunRequired ? "Review & publish" : "Publish";

  return (
    <>
      <Button
        variant={!running && gate.ready ? "default" : "outline"}
        onClick={() => setOpen(true)}
        aria-label={
          running
            ? "Publishing — read the remaining steps"
            : gate.ready
              ? `${label} — the gate passes`
              : `${label} — ${gate.blockers.length} required sections are not settled`
        }
      >
        {label}
      </Button>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent className="spec-publish-dialog">
          {running ? (
            <PublishedFace status={current} onClose={() => setOpen(false)} />
          ) : gate.ready ? (
            <ReadyFace
              gate={gate}
              status={current}
              acknowledged={acknowledged}
              onAcknowledge={setAcknowledged}
              pending={publish.isPending}
              error={error}
              refused={refused}
              onKeepDrafting={() => setOpen(false)}
              onPublish={() => {
                setError(null);
                publish.mutate(
                  {
                    acknowledgeOpenQuestions: acknowledged,
                    runGapCheck: gate.gapCheckRunRequired,
                  },
                  { onError: (cause) => absorb(cause, setRefusal, setError) },
                );
              }}
            />
          ) : (
            <BlockedFace
              gate={gate}
              status={current}
              pending={publish.isPending}
              error={error}
              refused={refused}
              onCancel={() => setOpen(false)}
              onReview={(sectionId) => {
                setOpen(false);
                onReviewSection(sectionId);
              }}
              onRecheck={() => {
                setError(null);
                publish.mutate(
                  { acknowledgeOpenQuestions: false, runGapCheck: gate.gapCheckRunRequired },
                  { onError: (cause) => absorb(cause, setRefusal, setError) },
                );
              }}
            />
          )}
        </DialogContent>
      </Dialog>
    </>
  );
}

/**
 * The pin refused an earlier publish. Amber, not red: nothing was lost and
 * nothing was published — the document moved out of the gate, and the person
 * decides what to do next.
 */
function RefusedNotice({ reason }: { reason: string }) {
  return (
    <p className="spec-publish-refused">
      <TriangleAlert aria-hidden="true" />
      The last publish did not pin: {reason} Nothing was published.
    </p>
  );
}

/** A refusal is the new gate, so the dialog shows it instead of a toast. */
function absorb(
  cause: unknown,
  setRefusal: (status: SpecPublishStatus | null) => void,
  setError: (message: string | null) => void,
): void {
  const refusal = specPublishRefusal(cause);
  if (refusal?.status) {
    setRefusal(refusal.status);
    // The gate itself states what is missing, so a second line would repeat it.
    setError(refusal.reason === "blocked" ? null : refusal.message);
    return;
  }
  setError(cause instanceof Error ? cause.message : "The publish did not run.");
}

function BlockedFace({
  gate,
  status,
  pending,
  error,
  refused,
  onCancel,
  onReview,
  onRecheck,
}: {
  gate: SpecPublishGate;
  status: SpecPublishStatus;
  pending: boolean;
  error: string | null;
  refused: string | null;
  onCancel: () => void;
  onReview: (sectionId: string) => void;
  onRecheck: () => void;
}) {
  const count = gate.blockers.length;
  return (
    <>
      <DialogHeader>
        <span className="spec-publish-eyebrow">publish gate</span>
        <DialogTitle>
          {count === 1
            ? "1 required section is not settled"
            : `${count} required sections are not settled`}
        </DialogTitle>
        <DialogDescription>
          Publishing pins an immutable version and opens ticketization. Every required section must
          be settled, or marked n/a with a reason.
        </DialogDescription>
      </DialogHeader>
      <ul className="spec-publish-blockers">
        {gate.blockers.map((blocker) => (
          <li key={blocker.sectionId}>
            <span className="spec-publish-blocker-copy">
              <CircleDotDashed aria-hidden="true" />
              <b>{blocker.sectionTitle}</b>
              <span>{blockerText(blocker.reason)}</span>
            </span>
            <Button size="sm" variant="outline" onClick={() => onReview(blocker.sectionId)}>
              Review →
            </Button>
          </li>
        ))}
      </ul>
      {refused ? <RefusedNotice reason={refused} /> : null}
      {error ? <p className="spec-publish-error">{error}</p> : null}
      <DialogFooter className="spec-publish-footer">
        <span className="spec-publish-meta">{footerMeta(gate, status)}</span>
        <Button size="sm" variant="ghost" onClick={onCancel}>
          Cancel
        </Button>
        {/* Quiet, because the gate does not pass. It re-checks: a section
            settled in another tab shows up here. */}
        <Button size="sm" variant="outline" disabled={pending} onClick={onRecheck}>
          {pending ? "Checking…" : "Publish"}
        </Button>
      </DialogFooter>
    </>
  );
}

function ReadyFace({
  gate,
  status,
  acknowledged,
  onAcknowledge,
  pending,
  error,
  refused,
  onKeepDrafting,
  onPublish,
}: {
  gate: SpecPublishGate;
  status: SpecPublishStatus;
  acknowledged: boolean;
  onAcknowledge: (value: boolean) => void;
  pending: boolean;
  error: string | null;
  refused: string | null;
  onKeepDrafting: () => void;
  onPublish: () => void;
}) {
  const questions = gate.openQuestions;
  const amber = questions.length > 0;
  return (
    <>
      <DialogHeader className={amber ? "spec-publish-amber" : undefined}>
        <span className="spec-publish-eyebrow">
          {amber ? (
            <>
              <TriangleAlert aria-hidden="true" /> publishing with open questions
            </>
          ) : (
            "publish gate"
          )}
        </span>
        <DialogTitle>
          {amber
            ? `${questions.length} ${questions.length === 1 ? "question goes" : "questions go"} into the tickets unanswered`
            : "This spec is ready to publish"}
        </DialogTitle>
        <DialogDescription>
          {amber
            ? "Open questions do not block a publish. They carry into the tickets that cover their sections."
            : "Every required section is settled, or marked n/a with a reason."}
        </DialogDescription>
      </DialogHeader>
      {amber ? (
        <>
          <ul className="spec-publish-questions">
            {questions.map((question, index) => (
              <li key={question.id}>
                <span className="spec-publish-flag" aria-hidden="true">
                  <Flag /> {index + 1}
                </span>
                <span>{question.text}</span>
                <span className="spec-publish-backlink">§{question.sectionTitle}</span>
              </li>
            ))}
          </ul>
          <label className="spec-publish-ack">
            <input
              type="checkbox"
              checked={acknowledged}
              onChange={(event) => onAcknowledge(event.target.checked)}
            />
            <span>
              I am publishing this spec with{" "}
              <b>
                {questions.length} open {questions.length === 1 ? "question" : "questions"}
              </b>{" "}
              unresolved. They carry into the tickets covering their sections.
            </span>
          </label>
        </>
      ) : null}
      {refused ? <RefusedNotice reason={refused} /> : null}
      {error ? <p className="spec-publish-error">{error}</p> : null}
      <p className="spec-publish-note">{footerMeta(gate, status)}</p>
      <DialogFooter className="spec-publish-footer">
        <span className="spec-publish-meta">
          <Lock aria-hidden="true" /> pins this version · one-way: ends drafting, spec stays
          readable
        </span>
        <Button size="sm" variant="ghost" onClick={onKeepDrafting}>
          Keep drafting
        </Button>
        <Button size="sm" disabled={pending || (amber && !acknowledged)} onClick={onPublish}>
          {pending
            ? gate.gapCheckRunRequired
              ? "Reviewing…"
              : "Publishing…"
            : gate.gapCheckRunRequired
              ? "Review & publish"
              : "Publish & ticketize"}
        </Button>
      </DialogFooter>
    </>
  );
}

/** What the person sees once the intent is recorded. The steps run behind it. */
function PublishedFace({ status, onClose }: { status: SpecPublishStatus; onClose: () => void }) {
  const publish = status.publish!;
  return (
    <>
      <DialogHeader>
        <span className="spec-publish-eyebrow">published</span>
        <DialogTitle>This spec is published</DialogTitle>
        <DialogDescription>
          The version is pinned and the spec is read-only. Rework means a new spec.
        </DialogDescription>
      </DialogHeader>
      <ol className="spec-publish-steps">
        <PublishStep
          done={publish.state !== "requested"}
          active={publish.state === "requested"}
          label="Pinned the checkpoint"
        />
        <PublishStep
          done={publish.state === "artifact_published" || publish.state === "complete"}
          active={publish.state === "pinned"}
          label={
            publish.artifactVersion === null
              ? "Writing the shared version"
              : `Wrote shared version ${publish.artifactVersion}`
          }
        />
        <PublishStep
          done={publish.state === "complete"}
          active={publish.state === "artifact_published"}
          label="Moved the session to ticketize"
        />
      </ol>
      {publish.acknowledgedQuestionCount > 0 ? (
        <p className="spec-publish-note">
          {publish.acknowledgedQuestionCount} open questions were carried into the tickets.
        </p>
      ) : null}
      {publish.lastError ? (
        <p className="spec-publish-error">
          The last step did not finish: {publish.lastError}. It retries on its own.
        </p>
      ) : null}
      <DialogFooter className="spec-publish-footer">
        <Button size="sm" variant="outline" onClick={onClose}>
          Done
        </Button>
      </DialogFooter>
    </>
  );
}

function PublishStep({ label, done, active }: { label: string; done?: boolean; active: boolean }) {
  return (
    <li className={done ? "is-done" : active ? "is-active" : undefined}>
      <span aria-hidden="true">{done ? "✓" : active ? "…" : "·"}</span>
      {label}
    </li>
  );
}

function blockerText(reason: SpecPublishGate["blockers"][number]["reason"]): string {
  switch (reason) {
    case "proposed":
      return "— proposed, not settled";
    case "open":
      return "— open";
    case "na_without_reason":
      return "— marked n/a with no reason";
  }
}

function footerMeta(gate: SpecPublishGate, status: SpecPublishStatus): string {
  const ready = `${gate.settledRequiredCount} of ${gate.requiredCount} ready`;
  if (!status.gapCheck.gates) return ready;
  if (status.gapCheck.ranAt === null) return `${ready} · review has not run`;
  if (status.gapCheck.stale) return `${ready} · review is stale`;
  return `${ready} · review ran ${formatTime(status.gapCheck.ranAt)}`;
}

function formatTime(iso: string): string {
  const parsed = new Date(iso);
  if (Number.isNaN(parsed.getTime())) return iso;
  return parsed.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
}
