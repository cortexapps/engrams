import type { SpecSurfaceSection } from "./spec-surface";

interface GlyphPresentation {
  glyph: "✓" | "◐" | "○" | "⚑" | "●";
  label: string;
  tone: "nominal" | "proposal" | "open" | "question" | "reading";
}

export function sectionGlyph(section: SpecSurfaceSection): GlyphPresentation {
  if (section.isBeingRead) return { glyph: "●", label: "Being read", tone: "reading" };
  // The state keeps its glyph even with open questions: the rail already
  // shows a ⚑n badge for those, and replacing the state glyph hid whether a
  // flagged section was settled.
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
