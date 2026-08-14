import type { ThreadMessageLike } from "@assistant-ui/react";

import type { SpecMessage } from "@/hooks/useSpecMessages";

export interface SpecHumanThreadEntry {
  kind: "human";
  id: string;
  promptId: string;
  author: SpecMessage["author"] | null;
  text: string;
  createdAt: string | null;
}

export interface SpecAgentThreadEntry {
  kind: "agent";
  id: string;
  text: string;
  citations: string[];
  createdAt: string | null;
}

export interface SpecDocumentActivityEntry {
  kind: "document_activity";
  id: string;
  sectionIds: string[];
  sectionTitles: string[];
  createdAt: string | null;
}

export type SpecThreadEntry =
  | SpecHumanThreadEntry
  | SpecAgentThreadEntry
  | SpecDocumentActivityEntry;

export interface ParsedSpecAgentText {
  text: string;
  citations: string[];
}

/**
 * The text of a human turn that has no row in the message store.
 *
 * Not every human turn arrives through the messages route. The problem
 * statement that creates a spec is sent with the session, and a block
 * conversation sends its own scoped prompt, so neither has a stored row — and
 * showing "This message is not available" for the sentence a person opened
 * their spec with makes the founding turn of every spec look broken.
 *
 * A turn carrying a `[speaker:` header is the one case that stays hidden. That
 * header is the agent's attribution seam, so rendering it would present a line
 * a member could have forged as though the UI vouched for it. Without a header
 * there is nothing to forge and the text is simply what the person wrote.
 */
function unattributedText(message: ThreadMessageLike): string {
  const parts = Array.isArray(message.content) ? message.content : [];
  const text = parts
    .map((part) => (typeof part === "object" && part.type === "text" ? part.text : ""))
    .join("")
    .trim();
  return text.length > 0 && !SPEAKER_HEADER.test(text) ? text : "";
}

/** Matches the attribution header the server prepends for the agent alone. */
const SPEAKER_HEADER = /^\[speaker:/m;

/**
 * Project the shared buildMessages fold through an explicit allow-list. No
 * unrecognized message role or part can enter the spec conversation.
 */
export function buildSpecThread(
  messages: readonly ThreadMessageLike[],
  messagesByPromptId: ReadonlyMap<string, SpecMessage>,
  sectionTitles: ReadonlyMap<string, string> = new Map(),
): SpecThreadEntry[] {
  const result: SpecThreadEntry[] = [];
  for (const [messageIndex, message] of messages.entries()) {
    const messageId = message.id ?? `message:${messageIndex}`;
    if (message.role === "user") {
      const promptId = messageId;
      const stored = messagesByPromptId.get(promptId);
      result.push({
        kind: "human",
        id: `human:${promptId}`,
        promptId,
        author: stored?.author ?? null,
        text: stored?.text ?? unattributedText(message),
        createdAt: stored?.createdAt ?? toIso(message.createdAt),
      });
      continue;
    }
    if (message.role !== "assistant" || !Array.isArray(message.content)) continue;
    let agentIndex = 0;
    for (const part of message.content) {
      if (part.type === "text" && typeof part.text === "string" && part.text.trim().length > 0) {
        const parsed = parseSpecAgentText(part.text);
        result.push({
          kind: "agent",
          id: `agent:${messageId}:${agentIndex++}`,
          text: parsed.text,
          citations: parsed.citations,
          createdAt: toIso(message.createdAt),
        });
      } else if (
        part.type === "tool-call" &&
        isSpecUpdateSectionTool(part.toolName) &&
        part.isError !== true &&
        specUpdateApplied(part.result)
      ) {
        const sectionId = readSectionId(part.args);
        if (sectionId !== null) {
          pushDocumentActivity(result, messageId, sectionId, sectionTitles, message.createdAt);
        }
      }
    }
  }
  return result;
}

/** Extract repository citations that the normal thread renders as code chips. */
export function parseSpecAgentText(text: string): ParsedSpecAgentText {
  const citations: string[] = [];
  const withoutCitations = text.replace(
    /`((?:[^`\n]+\s@\s[0-9a-f]{7,40})|(?:spec\s+[^`\n]+))`/gi,
    (_match, citation) => {
      if (!citations.includes(citation)) citations.push(citation);
      return "";
    },
  );
  return {
    text: withoutCitations
      .replace(/[ \t]+\n/g, "\n")
      .replace(/ {2,}/g, " ")
      .trim(),
    citations,
  };
}

function pushDocumentActivity(
  result: SpecThreadEntry[],
  messageId: string,
  sectionId: string,
  titles: ReadonlyMap<string, string>,
  createdAt: Date | undefined,
): void {
  const previous = result[result.length - 1];
  if (previous?.kind === "document_activity") {
    if (!previous.sectionIds.includes(sectionId)) {
      previous.sectionIds.push(sectionId);
      previous.sectionTitles.push(titles.get(sectionId) ?? sectionId);
    }
    return;
  }
  result.push({
    kind: "document_activity",
    id: `activity:${messageId}:${sectionId}`,
    sectionIds: [sectionId],
    sectionTitles: [titles.get(sectionId) ?? sectionId],
    createdAt: toIso(createdAt),
  });
}

function isSpecUpdateSectionTool(name: string): boolean {
  return name === "spec_update_section" || name === "mcp__engrams__spec_update_section";
}

function readSectionId(args: unknown): string | null {
  if (!isRecord(args)) return null;
  if (typeof args.section_id === "string") return args.section_id;
  return typeof args.sectionId === "string" ? args.sectionId : null;
}

function specUpdateApplied(result: unknown): boolean {
  let value = result;
  if (typeof value === "string") {
    try {
      value = JSON.parse(value) as unknown;
    } catch {
      return true;
    }
  }
  return !isRecord(value) || value.applied !== false;
}

function toIso(value: Date | undefined): string | null {
  return value instanceof Date && !Number.isNaN(value.valueOf()) ? value.toISOString() : null;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
