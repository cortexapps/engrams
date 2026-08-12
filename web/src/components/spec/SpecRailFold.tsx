import { FlagIcon, ListChecksIcon } from "lucide-react";
import { useState, type ReactNode } from "react";

import { Sheet, SheetContent, SheetTitle, SheetTrigger } from "@/components/ui/sheet";
import type { SpecRail } from "@/hooks/useSpecRead";

/** Every open question in the spec. These are the flags a reader came back for. */
export function openQuestionCount(rail: SpecRail): number {
  return rail.layers.reduce(
    (total, layer) =>
      total + layer.sections.reduce((sum, section) => sum + section.openQuestionCount, 0),
    0,
  );
}

export function railFoldLabel(complete: number, total: number, flags: number): string {
  const questions = flags === 1 ? "question" : "questions";
  return `Spec sections. ${complete} of ${total} complete. ${flags} open ${questions}.`;
}

/**
 * The section rail, folded for a small screen (ADR 0114 D4, R51).
 *
 * A small screen reads and resolves, so the rail gives up its column and keeps
 * only the two facts that make a reader open it: the tally and the flag count.
 * The full rail is one tap away in the sheet, and it keeps every action it has
 * on the desktop — a reader can still confirm a section from there.
 */
export function SpecRailFold({ rail, children }: { rail: SpecRail; children: ReactNode }) {
  const [open, setOpen] = useState(false);
  const { complete, total } = rail.completeness;
  const flags = openQuestionCount(rail);

  return (
    <Sheet open={open} onOpenChange={setOpen}>
      <SheetTrigger asChild>
        <button
          type="button"
          className="spec-rail-fold"
          aria-label={railFoldLabel(complete, total, flags)}
        >
          <ListChecksIcon aria-hidden="true" />
          <span className="spec-rail-fold-tally">
            {complete}/{total} sections
          </span>
          <span className="spec-rail-fold-flags" data-empty={flags === 0 ? "true" : undefined}>
            <FlagIcon aria-hidden="true" />
            {flags}
          </span>
        </button>
      </SheetTrigger>
      <SheetContent side="bottom" className="spec-rail-sheet">
        <SheetTitle className="spec-rail-sheet-title">Spec sections</SheetTitle>
        <div className="spec-rail-sheet-body">{children}</div>
      </SheetContent>
    </Sheet>
  );
}
