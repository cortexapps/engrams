import type { ReactNode } from "react";

import { Button } from "@/components/ui/button";
import { Text } from "@/components/ui/text";
import { NextProposalCard } from "./NextProposalCard";
import type { HumanPresence, SpecPresenceEntry } from "./section-presence";
import type { SpecSurface } from "./spec-surface";
import "./spec-mode.css";

export function SpecMobileView({
  title,
  surface,
  presence,
  onSend,
  onBackToPublished,
  children,
}: {
  title: string;
  surface: SpecSurface;
  presence: SpecPresenceEntry[];
  onSend: (message: string) => Promise<unknown>;
  onBackToPublished?: () => void;
  children: ReactNode;
}) {
  const person = presence.find((entry): entry is HumanPresence => entry.kind === "human");
  const located = presence.find(
    (entry): entry is HumanPresence => entry.kind === "human" && entry.sectionId !== undefined,
  );
  const location = located
    ? surface.sections.find((section) => section.id === located.sectionId)?.title
    : null;
  const questionCount = surface.sections.reduce(
    (total, section) => total + section.openQuestionCount,
    0,
  );

  return (
    <main className="spec-mode-mobile" aria-label="Spec on a small screen">
      <header className="spec-mode-mobile-header">
        <div>
          <Text as="h1" variant="heading">
            {title}
          </Text>
          <Text as="p" variant="code" tone="muted" className="spec-mode-mobile-status">
            {surface.settledCount} of {surface.totalCount} settled · ⚑{questionCount}
            {located && location
              ? ` · ${located.isSelf ? "You are" : `${located.name} is`} in §${location}`
              : ""}
          </Text>
        </div>
        {person ? (
          <span
            className="spec-mode-mobile-avatar"
            style={{ backgroundColor: person.color }}
            aria-label={person.isSelf ? "You are here" : `${person.name} is here`}
          >
            {initials(person.name)}
          </span>
        ) : null}
      </header>
      {onBackToPublished ? (
        <div className="spec-mode-mobile-back">
          <Button type="button" variant="ghost" onClick={onBackToPublished}>
            Back to published version
          </Button>
        </div>
      ) : null}
      <section className="spec-mode-mobile-document" aria-label="Spec document" data-scroll="doc">
        {children}
      </section>
      <div className="spec-mode-mobile-next">
        <NextProposalCard next={surface.next} onSend={onSend} alternateLabel="Reply instead" />
      </div>
    </main>
  );
}

function initials(name: string): string {
  return name
    .split(/\s+/)
    .filter(Boolean)
    .slice(0, 2)
    .map((part) => part[0]?.toUpperCase())
    .join("");
}
