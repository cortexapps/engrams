import {
  CheckIcon,
  CircleDotDashedIcon,
  MessageCircleQuestionIcon,
  RotateCcwIcon,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";

import { isSectionComplete } from "@/components/spec-mode/spec-surface";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Progress } from "@/components/ui/progress";
import { Textarea } from "@/components/ui/textarea";
import type { SpecRail, SpecRailSection } from "@/hooks/useSpecRead";

export interface SpecRailAction {
  sectionId: string;
  state: "open" | "proposed" | "settled" | "n/a";
  reason?: string;
}

export function SpecSectionRail({
  rail,
  editable,
  pendingSectionId,
  focusedSectionId = null,
  onAction,
}: {
  rail: SpecRail;
  editable: boolean;
  pendingSectionId: string | null;
  /** The section a publish blocker sent the person to (mock 2k). */
  focusedSectionId?: string | null;
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

      <ol className="spec-section-list">
        {rail.sections.map((section) => (
          <SectionRow
            key={section.id}
            section={section}
            editable={editable}
            pending={pendingSectionId === section.id}
            focused={focusedSectionId === section.id}
            onAction={onAction}
          />
        ))}
      </ol>
    </div>
  );
}

function SectionRow({
  section,
  editable,
  pending,
  focused,
  onAction,
}: {
  section: SpecRailSection;
  editable: boolean;
  pending: boolean;
  focused: boolean;
  onAction: (action: SpecRailAction) => void;
}) {
  const row = useRef<HTMLLIElement | null>(null);
  useEffect(() => {
    if (focused) row.current?.scrollIntoView({ block: "nearest" });
  }, [focused]);
  return (
    <li
      ref={row}
      id={`spec-section-row-${section.id}`}
      className={focused ? "is-focused" : undefined}
    >
      <div className="spec-section-row-main">
        <span className="spec-section-marker" aria-hidden="true">
          {isSectionComplete(section) ? <CheckIcon /> : <CircleDotDashedIcon />}
        </span>
        <div className="spec-section-copy">
          <div>
            <strong>{section.title}</strong>
          </div>
          <div className="spec-section-meta">
            <StateChip section={section} />
            {section.settledBy && (
              <span className="spec-settle-credit">Settled by {section.settledBy.name}</span>
            )}
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
          {section.state === "proposed" && (
            <>
              <Button
                type="button"
                variant="ghost"
                size="xs"
                disabled={pending}
                onClick={() => onAction({ sectionId: section.id, state: "settled" })}
              >
                Settle
              </Button>
              <Button
                type="button"
                variant="ghost"
                size="xs"
                disabled={pending}
                onClick={() => onAction({ sectionId: section.id, state: "open" })}
              >
                Drop
              </Button>
            </>
          )}
          {isSectionComplete(section) && (
            <Button
              type="button"
              variant="ghost"
              size="xs"
              disabled={pending}
              onClick={() => onAction({ sectionId: section.id, state: "proposed" })}
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
  if (section.state === "settled") {
    return <Badge className="spec-state-chip is-settled">Settled</Badge>;
  }
  if (section.state === "proposed") {
    return (
      <Badge variant="secondary" className="spec-state-chip">
        Proposed
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
    <Badge variant="outline" className="spec-state-chip is-open">
      Not started
    </Badge>
  );
}
