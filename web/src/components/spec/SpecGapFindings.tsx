import { Button } from "@/components/ui/button";
import type { GapDispositionAction, SpecGapFinding } from "@/hooks/useSpecGapCheck";

import "./spec-gap-check.css";

const KIND_LABEL: Record<SpecGapFinding["kind"], string> = {
  requirement_gap: "gap",
  scope_creep: "scope",
  speculative_machinery: "speculative",
  missing_outer_layer: "structure",
  no_requirements: "structure",
  red_team: "red-team",
};

const DISPOSITION_LABEL: Record<SpecGapFinding["disposition"], string> = {
  pending: "",
  question_opened: "question opened",
  diff_accepted: "diff accepted",
  dismissed: "dismissed",
};

export interface SpecGapFindingsProps {
  findings: readonly SpecGapFinding[];
  stoppedAtLayerKey: string | null;
  suppressedCount: number;
  editable: boolean;
  /** The spec moved on since this pass, so its diffs no longer describe it. */
  stale?: boolean;
  pendingFindingId?: string | null;
  onDispose: (findingId: string, action: GapDispositionAction) => void;
}

/**
 * The findings list (mock 2j). Each finding carries its own disposition:
 * open a question at the anchor, or accept the proposed diff. Nothing lands
 * without one of these clicks (R28).
 */
export function SpecGapFindings({
  findings,
  stoppedAtLayerKey,
  suppressedCount,
  editable,
  stale = false,
  pendingFindingId,
  onDispose,
}: SpecGapFindingsProps) {
  if (findings.length === 0) {
    return <p className="spec-gap-empty">This pass found nothing to report.</p>;
  }

  return (
    <div className="spec-gap-findings">
      {findings.map((finding, index) => (
        <Finding
          key={finding.id}
          finding={finding}
          ordinal={index + 1}
          stopped={finding.severity === "fatal" && finding.layerKey === stoppedAtLayerKey}
          editable={editable}
          stale={stale}
          busy={pendingFindingId === finding.id}
          onDispose={onDispose}
        />
      ))}
      {suppressedCount > 0 ? (
        <p className="spec-gap-suppressed">
          {suppressedCount} finding{suppressedCount === 1 ? "" : "s"} below this layer are withheld.
          Resolve the flaw above first, then run the check again.
        </p>
      ) : null}
    </div>
  );
}

function Finding({
  finding,
  ordinal,
  stopped,
  editable,
  stale,
  busy,
  onDispose,
}: {
  finding: SpecGapFinding;
  ordinal: number;
  stopped: boolean;
  editable: boolean;
  stale: boolean;
  busy: boolean;
  onDispose: (findingId: string, action: GapDispositionAction) => void;
}) {
  const disposed = finding.disposition !== "pending";
  const classes = ["spec-gap-finding"];
  if (stopped) classes.push("spec-gap-finding-fatal");
  if (disposed) classes.push("spec-gap-finding-disposed");

  return (
    <div className={classes.join(" ")} role="group" aria-label={`Finding ${ordinal}`}>
      <span>
        <span className="spec-gap-finding-label">
          {stopped
            ? `${KIND_LABEL[finding.kind]} · stopped outside-in`
            : `finding ${ordinal} · ${KIND_LABEL[finding.kind]}`}
        </span>
        {finding.detail}
      </span>
      {disposed ? (
        <span className="spec-gap-finding-outcome">{DISPOSITION_LABEL[finding.disposition]}</span>
      ) : editable ? (
        <span className="spec-gap-finding-actions">
          <Button
            size="sm"
            variant="outline"
            disabled={busy}
            onClick={() => onDispose(finding.id, "open_question")}
          >
            Open question @ §{finding.sectionTitle}
          </Button>
          {finding.proposedDiff === null ? null : (
            <Button
              size="sm"
              // A diff written against an older revision would replace the
              // whole section and discard the edits made since.
              disabled={busy || stale}
              title={stale ? "The spec changed since this pass. Run the check again." : undefined}
              onClick={() => onDispose(finding.id, "accept_diff")}
            >
              Accept proposed diff
            </Button>
          )}
          <Button
            size="sm"
            variant="ghost"
            disabled={busy}
            onClick={() => onDispose(finding.id, "dismiss")}
          >
            Dismiss
          </Button>
        </span>
      ) : null}
    </div>
  );
}
