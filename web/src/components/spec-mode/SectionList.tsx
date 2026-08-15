import { Text } from "@/components/ui/text";
import { SectionGlyph } from "./SectionGlyph";
import type { HumanPresence, SpecPresenceEntry } from "./section-presence";
import type { SpecSurface } from "./spec-surface";

export function SectionList({
  surface,
  presence = [],
  onSelectSection,
}: {
  surface: SpecSurface;
  presence?: SpecPresenceEntry[];
  onSelectSection: (sectionId: string) => void;
}) {
  return (
    <div className="spec-mode-section-list-shell">
      <header className="spec-mode-section-list-header">
        <Text as="span" variant="label" tone="muted">
          This spec
        </Text>
        <div className="spec-mode-section-tally">
          <Text as="strong" variant="stat">
            {surface.settledCount}
          </Text>
          <Text as="span" variant="code" tone="muted">
            of {surface.totalCount} settled
          </Text>
        </div>
      </header>

      <ol className="spec-mode-section-list">
        {surface.sections.map((section) => {
          const people = presence.filter(
            (entry): entry is HumanPresence =>
              entry.kind === "human" && entry.sectionId === section.id,
          );
          return (
            <li key={section.id}>
              <button
                type="button"
                className="spec-mode-section-row"
                data-state={section.state}
                data-reached={section.isReached ? "true" : "false"}
                data-reading={section.isBeingRead ? "true" : undefined}
                onClick={() => onSelectSection(section.id)}
              >
                <SectionGlyph section={section} />
                <span className="spec-mode-section-row-copy">
                  <Text as="span" variant="body" className="spec-mode-section-row-title">
                    {section.title}
                  </Text>
                  {section.state === "settled" && section.credit ? (
                    <Text
                      as="span"
                      variant="code"
                      tone="muted"
                      className="spec-mode-section-credit"
                    >
                      Settled by {section.credit.by.name}
                    </Text>
                  ) : null}
                </span>
                {section.openQuestionCount > 0 ? (
                  <Text
                    as="span"
                    variant="code"
                    className="spec-mode-section-flags"
                    aria-label={`${section.openQuestionCount} open ${section.openQuestionCount === 1 ? "question" : "questions"}`}
                  >
                    ⚑{section.openQuestionCount}
                  </Text>
                ) : null}
                {people.length > 0 ? (
                  <span className="spec-mode-section-presence-dots">
                    {people.map((person) => (
                      <span
                        key={person.clientId}
                        className="spec-mode-section-presence-dot"
                        style={{ backgroundColor: person.color }}
                        aria-label={`${person.isSelf ? "You are" : `${person.name} is`} in ${section.title}`}
                      />
                    ))}
                  </span>
                ) : null}
              </button>
            </li>
          );
        })}
      </ol>

      <footer className="spec-mode-section-legend">
        <Text as="span" variant="label" tone="muted">
          Legend
        </Text>
        <Text as="span" variant="code" tone="muted">
          ✓ settled · ◐ proposed · ○ open · ● reading · ⚑ questions
        </Text>
      </footer>
    </div>
  );
}
