import { useMemo } from "react";

import { buildMessages } from "@/components/session-thread/buildMessages";
import { useSpecEvents } from "@/hooks/useSpecEvents";
import { useSpecMessages, type SpecMessage } from "@/hooks/useSpecMessages";
import type { IndexedEvent } from "@/lib/types";
import { buildSpecThread } from "./buildSpecThread";

const EMPTY_MESSAGES = new Map();
const EMPTY_SECTION_TITLES = new Map<string, string>();

export function useSpecConversation(
  specId: string,
  sectionTitles: ReadonlyMap<string, string> = EMPTY_SECTION_TITLES,
  owner: SpecMessage["author"] | null = null,
) {
  const eventState = useSpecEvents(specId);
  const messageState = useSpecMessages(specId);
  const folded = useMemo(
    () => buildMessages(eventState.events, specId, undefined, eventState.streamingText),
    [eventState.events, eventState.streamingText, specId],
  );
  const messagesByPromptId = messageState.data?.byPromptId ?? EMPTY_MESSAGES;
  const entries = useMemo(
    () => buildSpecThread(folded.messages, messagesByPromptId, sectionTitles, owner),
    [folded.messages, messagesByPromptId, owner, sectionTitles],
  );
  const acknowledgedPromptIds = useMemo(
    () => new Set(messageState.data?.messages.map((message) => message.promptId) ?? []),
    [messageState.data?.messages],
  );
  const toolLabel = useMemo(() => currentToolLabel(eventState.events), [eventState.events]);

  return {
    entries,
    acknowledgedPromptIds,
    isRunning: folded.isRunning,
    toolLabel,
    hasError: Boolean(messageState.error || eventState.error),
  };
}

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
