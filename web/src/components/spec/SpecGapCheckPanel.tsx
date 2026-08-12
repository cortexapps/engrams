import { useState } from "react";

import { Button } from "@/components/ui/button";
import {
  useDisposeSpecGapFinding,
  useRunSpecGapCheck,
  useSpecGapCheck,
  type GapDispositionAction,
} from "@/hooks/useSpecGapCheck";

import { SpecGapFindings } from "./SpecGapFindings";
import { SpecTraceabilityMatrix } from "./SpecTraceabilityMatrix";
import "./spec-gap-check.css";

export interface SpecGapCheckPanelProps {
  specId: string;
  /** A published spec is read-only, so its findings cannot be disposed of. */
  editable: boolean;
  onBack: () => void;
}

/**
 * The gap-check surface (mock 2j): the traceability matrix, then the findings
 * with their one-click dispositions. The pass reports; a person decides.
 */
export function SpecGapCheckPanel({ specId, editable, onBack }: SpecGapCheckPanelProps) {
  const status = useSpecGapCheck(specId);
  const runGapCheck = useRunSpecGapCheck(specId);
  const dispose = useDisposeSpecGapFinding(specId);
  const [error, setError] = useState<string | null>(null);
  const [pendingFindingId, setPendingFindingId] = useState<string | null>(null);

  const run = status.data?.run ?? null;
  const stale = status.data?.stale ?? false;

  const onDispose = (findingId: string, action: GapDispositionAction) => {
    if (!run) return;
    setError(null);
    setPendingFindingId(findingId);
    dispose.mutate(
      { runId: run.id, findingId, action },
      {
        onError: (cause) => setError(cause.message),
        onSettled: () => setPendingFindingId(null),
      },
    );
  };

  return (
    <section className="spec-gap-check" aria-label="Gap check">
      <div className="spec-gap-check-head">
        <span className="spec-gap-check-title">Gap check</span>
        <span className="spec-gap-check-meta">
          {run ? (
            <span>{formatTime(run.createdAt)} · traceability + red-team</span>
          ) : (
            <span>not run yet</span>
          )}
          {run && stale ? <span>stale · the spec moved on</span> : null}
        </span>
        <span className="spec-gap-check-actions">
          {editable ? (
            <Button
              size="sm"
              variant={run === null || stale ? "default" : "outline"}
              disabled={runGapCheck.isPending}
              onClick={() => {
                setError(null);
                runGapCheck.mutate(
                  { actionId: crypto.randomUUID() },
                  { onError: (cause) => setError(cause.message) },
                );
              }}
            >
              {runGapCheck.isPending ? "Running…" : run === null ? "Run gap check" : "Run again"}
            </Button>
          ) : null}
          <Button size="sm" variant="outline" onClick={onBack}>
            Back to spec
          </Button>
        </span>
      </div>

      {status.isPending ? <p className="spec-gap-empty">Loading the last pass…</p> : null}

      {run === null && !status.isPending ? (
        <p className="spec-gap-empty">
          No pass has run for this spec. The check traces every requirement to the layers below it
          and flags content that cites none.
        </p>
      ) : null}

      {run ? (
        <>
          {run.stoppedAtLayerKey === null ? null : (
            <p className="spec-gap-suppressed">
              A fatal flaw stopped this pass at the {run.stoppedAtLayerKey} layer. Fix it before you
              read anything below it.
            </p>
          )}
          <SpecTraceabilityMatrix matrix={run.matrix} />
          <SpecGapFindings
            findings={run.findings}
            stoppedAtLayerKey={run.stoppedAtLayerKey}
            suppressedCount={run.suppressedCount}
            editable={editable}
            stale={stale}
            pendingFindingId={pendingFindingId}
            onDispose={onDispose}
          />
        </>
      ) : null}

      {error ? <p className="spec-action-error">{error}</p> : null}
    </section>
  );
}

function formatTime(iso: string): string {
  const parsed = new Date(iso);
  if (Number.isNaN(parsed.getTime())) return iso;
  return parsed.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
}
