import {
  CheckIcon,
  CircleDotDashedIcon,
  MessageCircleQuestionIcon,
  RotateCcwIcon,
} from "lucide-react";
import { useState } from "react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Progress } from "@/components/ui/progress";
import { Textarea } from "@/components/ui/textarea";
import type { SpecRail, SpecRailSection } from "@/hooks/useSpecRead";

export interface SpecRailAction {
  sectionId: string;
  state: "drafted" | "confirmed" | "n/a";
  reason?: string;
}

export function SpecSectionRail({
  rail,
  editable,
  pendingSectionId,
  onAction,
}: {
  rail: SpecRail;
  editable: boolean;
  pendingSectionId: string | null;
  onAction: (action: SpecRailAction) => void;
}) {
  const { complete, total } = rail.completeness;
  const percent = total === 0 ? 100 : Math.round((complete / total) * 100);

  return (
    <div className="spec-section-rail" aria-label="Spec sections">
      <div className="spec-completeness">
        <div>
          <strong>Completeness</strong>
          <span>
            {complete} of {total} complete
          </span>
        </div>
        <span className="spec-completeness-value">{percent}%</span>
        <Progress value={percent} aria-label={`${percent}% complete`} />
      </div>

      <div className="spec-layer-list">
        {rail.layers.map((layer, layerIndex) => {
          const layerComplete = layer.sections.filter(isComplete).length;
          return (
            <section
              className="spec-layer"
              aria-labelledby={`spec-layer-${layer.key}`}
              key={layer.key}
            >
              <header>
                <span className="spec-layer-index">{String(layerIndex + 1).padStart(2, "0")}</span>
                <div>
                  <h3 id={`spec-layer-${layer.key}`}>{layer.title}</h3>
                  {layer.description && <p>{layer.description}</p>}
                </div>
                <span className="spec-layer-count">
                  {layerComplete}/{layer.sections.length}
                </span>
              </header>
              <ol>
                {layer.sections.map((section) => (
                  <SectionRow
                    key={section.id}
                    section={section}
                    editable={editable}
                    pending={pendingSectionId === section.id}
                    onAction={onAction}
                  />
                ))}
              </ol>
            </section>
          );
        })}
      </div>
    </div>
  );
}

function SectionRow({
  section,
  editable,
  pending,
  onAction,
}: {
  section: SpecRailSection;
  editable: boolean;
  pending: boolean;
  onAction: (action: SpecRailAction) => void;
}) {
  return (
    <li className={section.frontier ? "is-frontier" : undefined}>
      <div className="spec-section-row-main">
        <span className="spec-section-marker" aria-hidden="true">
          {isComplete(section) ? <CheckIcon /> : <CircleDotDashedIcon />}
        </span>
        <div className="spec-section-copy">
          <div>
            <strong>{section.title}</strong>
            {section.frontier && <span className="spec-frontier-label">Frontier</span>}
          </div>
          <div className="spec-section-meta">
            <StateChip section={section} />
            {section.provisional && <Badge variant="outline">Provisional</Badge>}
            {section.openQuestionCount > 0 && (
              <span
                className="spec-question-count"
                aria-label={`${section.openQuestionCount} open ${section.openQuestionCount === 1 ? "question" : "questions"}`}
              >
                <MessageCircleQuestionIcon aria-hidden="true" />
                {section.openQuestionCount}
              </span>
            )}
          </div>
        </div>
      </div>
      {editable && (
        <div className="spec-section-actions">
          {section.state === "drafted" && (
            <Button
              type="button"
              variant="ghost"
              size="xs"
              disabled={pending}
              onClick={() => onAction({ sectionId: section.id, state: "confirmed" })}
            >
              Confirm
            </Button>
          )}
          {(section.state === "confirmed" || section.state === "n/a") && (
            <Button
              type="button"
              variant="ghost"
              size="xs"
              disabled={pending}
              onClick={() => onAction({ sectionId: section.id, state: "drafted" })}
            >
              <RotateCcwIcon aria-hidden="true" />
              Revisit
            </Button>
          )}
          {section.allowNa && section.state !== "n/a" && (
            <NotApplicableAction section={section} pending={pending} onAction={onAction} />
          )}
          {pending && <span className="spec-action-pending">Updating…</span>}
        </div>
      )}
    </li>
  );
}

function NotApplicableAction({
  section,
  pending,
  onAction,
}: {
  section: SpecRailSection;
  pending: boolean;
  onAction: (action: SpecRailAction) => void;
}) {
  const [open, setOpen] = useState(false);
  const [reason, setReason] = useState("");
  const valid = reason.trim().length > 0;
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button type="button" variant="ghost" size="xs" disabled={pending}>
          Mark n/a
        </Button>
      </PopoverTrigger>
      <PopoverContent align="end" className="spec-na-popover">
        <label htmlFor={`na-reason-${section.id}`}>Why is this section not applicable?</label>
        <Textarea
          id={`na-reason-${section.id}`}
          value={reason}
          onChange={(event) => setReason(event.target.value)}
          placeholder="State the reason"
        />
        <Button
          type="button"
          size="sm"
          disabled={!valid}
          onClick={() => {
            if (!valid) return;
            onAction({ sectionId: section.id, state: "n/a", reason: reason.trim() });
            setOpen(false);
          }}
        >
          Mark not applicable
        </Button>
      </PopoverContent>
    </Popover>
  );
}

function StateChip({ section }: { section: SpecRailSection }) {
  if (section.state === "confirmed") {
    return <Badge className="spec-state-chip is-confirmed">Confirmed</Badge>;
  }
  if (section.state === "drafted") {
    return (
      <Badge variant="secondary" className="spec-state-chip">
        Drafted
      </Badge>
    );
  }
  if (section.state === "n/a") {
    return (
      <Badge variant="outline" className="spec-state-chip" title={section.naReason ?? undefined}>
        N/a
      </Badge>
    );
  }
  return (
    <Badge variant="outline" className="spec-state-chip is-empty">
      Not started
    </Badge>
  );
}

function isComplete(section: SpecRailSection): boolean {
  return section.state === "confirmed" || section.state === "n/a";
}
