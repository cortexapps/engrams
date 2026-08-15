import { useMemo, useState } from "react";

import { Text } from "@/components/ui/text";
import { useSendSpecMessage, type SpecMessage } from "@/hooks/useSpecMessages";
import { NextProposalCard } from "./NextProposalCard";
import { SpecComposer } from "./SpecComposer";
import type { SpecPresenceEntry } from "./section-presence";
import type { SpecSurface } from "./spec-surface";
import { SpecThread, type PendingSpecMessage } from "./SpecThread";
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

interface InFlightSend {
  key: string;
  text: string;
  promptId: string | null;
}

export function ConversationRail({
  specId,
  surface,
  presence = [],
  owner = null,
  onSelectSection,
}: {
  specId: string;
  surface: SpecSurface;
  presence?: SpecPresenceEntry[];
  owner?: SpecMessage["author"] | null;
  onSelectSection: (sectionId: string) => void;
}) {
  const composerSend = useSendSpecMessage(specId);
  const proposalSend = useSendSpecMessage(specId);
  const sectionTitles = useMemo(
    () => new Map(surface.sections.map((section) => [section.id, section.title])),
    [surface.sections],
  );
  const conversation = useSpecConversation(specId, sectionTitles, owner);

  // A sent message renders immediately as a pending bubble and stays until
  // the shared conversation echoes it back. Without this the send gave no
  // sign at all, and people re-clicked — the message store carries a real
  // double-send from the first live drive.
  const [inFlight, setInFlight] = useState<InFlightSend[]>([]);
  const viewerName =
    presence.find(
      (entry): entry is Extract<SpecPresenceEntry, { kind: "human" }> =>
        entry.kind === "human" && entry.isSelf,
    )?.name ?? "You";
  const send =
    (deliver: (message: string) => Promise<{ promptId: string }>) => async (message: string) => {
      const key = `${Date.now()}:${Math.random().toString(36).slice(2)}`;
      setInFlight((current) => [...current, { key, text: message, promptId: null }]);
      try {
        const result = await deliver(message);
        setInFlight((current) =>
          current.map((entry) =>
            entry.key === key ? { ...entry, promptId: result.promptId } : entry,
          ),
        );
        return result;
      } catch (error) {
        setInFlight((current) => current.filter((entry) => entry.key !== key));
        throw error;
      }
    };
  const pending: PendingSpecMessage[] = inFlight
    .filter(
      (entry) => entry.promptId === null || !conversation.acknowledgedPromptIds.has(entry.promptId),
    )
    .filter(
      (entry) =>
        entry.promptId === null ||
        !conversation.entries.some(
          (candidate) => candidate.kind === "human" && candidate.promptId === entry.promptId,
        ),
    )
    .map((entry) => ({ key: entry.key, text: entry.text, authorName: viewerName }));

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
        pending={pending}
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
        onSend={send((message) => proposalSend.mutateAsync(message))}
      />
      <SpecComposer
        acknowledgedPromptIds={conversation.acknowledgedPromptIds}
        onSend={send((message) => composerSend.mutateAsync(message))}
      />
    </>
  );
}
