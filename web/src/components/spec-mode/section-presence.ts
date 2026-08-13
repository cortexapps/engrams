import type { Node as ProseMirrorNode } from "@tiptap/pm/model";
import type { Awareness } from "y-protocols/awareness";

export interface HumanPresence {
  kind: "human";
  clientId: number;
  id?: string;
  name: string;
  color: string;
}

export interface AgentPresence {
  kind: "agent";
  clientId: number;
  name: string;
  sessionId: string;
  toolCallId: string;
  sectionId: string;
}

export type SpecPresenceEntry = HumanPresence | AgentPresence;

export interface SectionPresenceEntry {
  clientId: number;
  userId?: string;
  name: string;
  color: string;
  sectionId: string;
}

export interface AwarenessCursor {
  anchor: unknown;
  head: unknown;
}

export type AwarenessCursorPosition = (cursor: AwarenessCursor, clientId: number) => number | null;

/** Find the top-level section that contains a ProseMirror position. */
export function sectionIdAtPos(doc: ProseMirrorNode, pos: number): string | null {
  if (!Number.isInteger(pos) || pos < 0 || pos > doc.content.size) return null;
  let sectionId: string | null = null;
  doc.forEach((node, offset) => {
    if (
      sectionId === null &&
      node.type.name === "section" &&
      typeof node.attrs.id === "string" &&
      pos >= offset + 1 &&
      pos < offset + node.nodeSize
    ) {
      sectionId = node.attrs.id;
    }
  });
  return sectionId;
}

/**
 * Map human awareness cursors to their current sections. The caller supplies
 * the editor binding's relative-to-absolute resolver, because only that
 * binding owns the Yjs-to-ProseMirror mapping.
 */
export function mapAwarenessCursorsToSections(
  awareness: Awareness,
  doc: ProseMirrorNode,
  cursorPosition: AwarenessCursorPosition,
): SectionPresenceEntry[] {
  const result: SectionPresenceEntry[] = [];
  for (const [clientId, state] of awareness.getStates()) {
    if (!isRecord(state.user) || typeof state.user.name !== "string") continue;
    const cursor = readCursor(state.cursor);
    if (cursor === null) continue;
    const position = cursorPosition(cursor, clientId);
    if (position === null) continue;
    const sectionId = sectionIdAtPos(doc, position);
    if (sectionId === null) continue;
    result.push({
      clientId,
      ...(typeof state.user.id === "string" ? { userId: state.user.id } : {}),
      name: state.user.name,
      color: typeof state.user.color === "string" ? state.user.color : "#64748b",
      sectionId,
    });
  }
  return result;
}

/** Read all human and synthesized agent presence from one awareness store. */
export function readSpecPresence(awareness: Awareness): SpecPresenceEntry[] {
  const entries: SpecPresenceEntry[] = [];
  for (const [clientId, state] of awareness.getStates()) {
    if (isRecord(state.user) && typeof state.user.name === "string") {
      entries.push({
        kind: "human",
        clientId,
        ...(typeof state.user.id === "string" ? { id: state.user.id } : {}),
        name: state.user.name,
        color: typeof state.user.color === "string" ? state.user.color : "#64748b",
      });
    }
    if (Array.isArray(state.agentPresence)) {
      for (const agent of state.agentPresence) {
        if (
          isRecord(agent) &&
          typeof agent.sectionId === "string" &&
          typeof agent.sessionId === "string" &&
          typeof agent.toolCallId === "string"
        ) {
          entries.push({
            kind: "agent",
            clientId,
            name: typeof agent.name === "string" ? agent.name : "engram",
            sectionId: agent.sectionId,
            sessionId: agent.sessionId,
            toolCallId: agent.toolCallId,
          });
        }
      }
    }
  }
  return entries;
}

function readCursor(value: unknown): AwarenessCursor | null {
  return isRecord(value) && "anchor" in value && "head" in value
    ? { anchor: value.anchor, head: value.head }
    : null;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
