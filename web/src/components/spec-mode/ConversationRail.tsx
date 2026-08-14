import { useMemo } from "react";

import { buildMessages } from "@/components/session-thread/buildMessages";
import { Text } from "@/components/ui/text";
import { useSpecEvents } from "@/hooks/useSpecEvents";
import { useSendSpecMessage, useSpecMessages } from "@/hooks/useSpecMessages";
import type { IndexedEvent } from "@/lib/types";
import { buildSpecThread } from "./buildSpecThread";
import { NextProposalCard } from "./NextProposalCard";
import { SpecComposer } from "./SpecComposer";
import type { SpecSurface } from "./spec-surface";
import { SpecThread } from "./SpecThread";

export function ConversationRail({
  specId,
  surface,
  onSelectSection,
}: {
  specId: string;
  surface: SpecSurface;
  onSelectSection: (sectionId: string) => void;
}) {
  const eventState = useSpecEvents(specId);
  const messageState = useSpecMessages(specId);
  const composerSend = useSendSpecMessage(specId);
  const proposalSend = useSendSpecMessage(specId);
  const folded = useMemo(
    () => buildMessages(eventState.events, specId, undefined, eventState.streamingText),
    [eventState.events, eventState.streamingText, specId],
  );
  const sectionTitles = useMemo(
    () => new Map(surface.sections.map((section) => [section.id, section.title])),
    [surface.sections],
  );
  const messagesByPromptId = messageState.data?.byPromptId ?? EMPTY_MESSAGES;
  const entries = useMemo(
    () => buildSpecThread(folded.messages, messagesByPromptId, sectionTitles),
    [folded.messages, messagesByPromptId, sectionTitles],
  );
  const acknowledgedPromptIds = useMemo(
    () => new Set(messageState.data?.messages.map((message) => message.promptId) ?? []),
    [messageState.data?.messages],
  );
  const toolLabel = useMemo(() => currentToolLabel(eventState.events), [eventState.events]);

  return (
    <>
      <header className="spec-mode-conversation-header">
        <Text as="h2" variant="label" tone="muted">
          Conversation
        </Text>
        <Text as="span" variant="code" tone="muted">
          members + engram
        </Text>
      </header>
      <SpecThread
        entries={entries}
        isRunning={folded.isRunning}
        toolLabel={toolLabel}
        onActivity={onSelectSection}
      />
      {messageState.error || eventState.error ? (
        <Text as="div" className="spec-mode-conversation-warning" tone="destructive" role="status">
          Conversation updates are reconnecting.
        </Text>
      ) : null}
      <NextProposalCard
        next={surface.next}
        onSend={(message) => proposalSend.mutateAsync(message)}
      />
      <SpecComposer
        acknowledgedPromptIds={acknowledgedPromptIds}
        onSend={(message) => composerSend.mutateAsync(message)}
      />
    </>
  );
}

const EMPTY_MESSAGES = new Map();

export function currentToolLabel(events: readonly IndexedEvent[]): string | null {
  const active = new Map<string, string>();
  for (const { event } of events) {
    if (event.type === "tool_call_started") {
      active.delete(event.tool_call_id);
      active.set(event.tool_call_id, event.tool_name);
    } else if (event.type === "tool_call_requested") {
      active.delete(event.tool_call_id);
      active.set(event.tool_call_id, event.name);
    } else if (event.type === "tool_call_completed" || event.type === "tool_result_submitted") {
      active.delete(event.tool_call_id);
    } else if (event.type === "run_completed" || event.type === "run_interrupted") {
      active.clear();
    }
  }
  const name = [...active.values()].at(-1);
  return name ? toolLabel(name) : null;
}

function toolLabel(rawName: string): string {
  const name = rawName.replace(/^mcp__engrams__/, "").toLowerCase();
  if (name === "spec_update_section" || name === "spec_update_block") {
    return "Updating the document";
  }
  if (name === "spec_gap_check") return "Looking for what breaks";
  if (/^(read|grep|glob|search|find|ls|web)/.test(name)) return "Reading the repository";
  if (/^(bash|shell|exec|run)/.test(name)) return "Checking the repository";
  const words = name.replaceAll("_", " ").replaceAll("-", " ");
  return `Using ${words}`;
}
