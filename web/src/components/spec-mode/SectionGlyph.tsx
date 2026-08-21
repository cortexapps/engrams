import type { SpecSurfaceSection } from "./spec-surface";

interface GlyphPresentation {
  glyph: "✓" | "◐" | "○" | "⚑";
  label: string;
  tone: "nominal" | "proposal" | "open" | "question";
}

export function sectionGlyph(section: SpecSurfaceSection): GlyphPresentation {
  // The state keeps its glyph even with open questions: the rail already
  // shows a ⚑n badge for those, and replacing the state glyph hid whether a
  // flagged section was settled.
  //
  // The same rule now covers where the reader is. `isBeingRead` is local
  // scroll position, not another person, and it used to take the glyph slot
  // outright — so the section you were looking at was the one whose state you
  // could not see, and its ● was a shape away from ◐ "proposed". The row
  // already carries a background for it, and `aria-current` names it.
  if (section.state === "settled" || section.state === "n/a") {
    return { glyph: "✓", label: "Settled", tone: "nominal" };
  }
  if (section.state === "proposed") {
    return { glyph: "◐", label: "Proposed", tone: "proposal" };
  }
  if (section.openQuestionCount > 0) {
    return { glyph: "⚑", label: "Has open questions", tone: "question" };
  }
  return { glyph: "○", label: "Open", tone: "open" };
}

export function SectionGlyph({ section }: { section: SpecSurfaceSection }) {
  const presentation = sectionGlyph(section);
  return (
    <span
      className="spec-mode-section-glyph"
      data-glyph-tone={presentation.tone}
      data-proposed={section.state === "proposed" ? "true" : undefined}
      role="img"
      aria-label={presentation.label}
    >
      {presentation.glyph}
    </span>
  );
}
