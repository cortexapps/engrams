import { useMemo } from "react";

import { Text } from "@/components/ui/text";
import { useSendSpecMessage } from "@/hooks/useSpecMessages";
import { NextProposalCard } from "./NextProposalCard";
import { SpecComposer } from "./SpecComposer";
import type { SpecPresenceEntry } from "./section-presence";
import type { SpecSurface } from "./spec-surface";
import { SpecThread } from "./SpecThread";
import { useSpecConversation } from "./useSpecConversation";

export { currentToolLabel } from "./useSpecConversation";

/**
 * Who is in the conversation, counted rather than asserted. This read
 * "members + engram" whatever was true, which told a person nothing and was
 * wrong whenever they were alone.
 */
function participantSummary(presence: readonly SpecPresenceEntry[]): string {
  const people = presence.filter((entry) => entry.kind === "human").length;
  const agent = presence.some((entry) => entry.kind === "agent");
  const who = people <= 1 ? "you" : people === 2 ? "you + 1 other" : `you + ${people - 1} others`;
  return agent ? `${who} + engram here` : `${who} here`;
}

export function ConversationRail({
  specId,
  surface,
  presence = [],
  onSelectSection,
}: {
  specId: string;
  surface: SpecSurface;
  presence?: SpecPresenceEntry[];
  onSelectSection: (sectionId: string) => void;
}) {
  const composerSend = useSendSpecMessage(specId);
  const proposalSend = useSendSpecMessage(specId);
  const sectionTitles = useMemo(
    () => new Map(surface.sections.map((section) => [section.id, section.title])),
    [surface.sections],
  );
  const conversation = useSpecConversation(specId, sectionTitles);

  return (
    <>
      <header className="spec-mode-conversation-header">
        <Text as="h2" variant="label" tone="muted">
          Conversation
        </Text>
        <Text as="span" variant="code" tone="muted">
          {participantSummary(presence)}
        </Text>
      </header>
      <SpecThread
        entries={conversation.entries}
        isRunning={conversation.isRunning}
        toolLabel={conversation.toolLabel}
        onActivity={onSelectSection}
      />
      {conversation.hasError ? (
        <Text as="div" className="spec-mode-conversation-warning" tone="destructive" role="status">
          Conversation updates are reconnecting.
        </Text>
      ) : null}
      <NextProposalCard
        next={surface.next}
        onSend={(message) => proposalSend.mutateAsync(message)}
      />
      <SpecComposer
        acknowledgedPromptIds={conversation.acknowledgedPromptIds}
        onSend={(message) => composerSend.mutateAsync(message)}
      />
    </>
  );
}
