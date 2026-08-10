import { useEffect, useMemo, useState } from "react";
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

export function SpecPresence({ awareness }: { awareness: Awareness }) {
  const [revision, setRevision] = useState(0);
  useEffect(() => {
    const update = () => setRevision((value) => value + 1);
    awareness.on("change", update);
    return () => awareness.off("change", update);
  }, [awareness]);
  const entries = useMemo(() => readSpecPresence(awareness), [awareness, revision]);

  return (
    <div className="spec-presence" aria-label={`${entries.length} participants present`}>
      <div className="spec-presence-avatars" aria-hidden="true">
        {entries.map((entry) => (
          <span
            key={`${entry.kind}-${entry.clientId}-${entry.kind === "agent" ? entry.toolCallId : (entry.id ?? entry.name)}`}
            className={`spec-presence-avatar ${entry.kind === "agent" ? "is-agent" : ""}`}
            style={entry.kind === "human" ? { backgroundColor: entry.color } : undefined}
          >
            {entry.kind === "agent" ? "✦" : initials(entry.name)}
          </span>
        ))}
      </div>
      <div className="spec-presence-copy">
        {entries.length === 0 && <span>No one else is here</span>}
        {entries.map((entry) =>
          entry.kind === "agent" ? (
            <span key={`copy-agent-${entry.clientId}-${entry.toolCallId}`}>
              {entry.name} ✎ §{entry.sectionId}
            </span>
          ) : (
            <span key={`copy-human-${entry.clientId}`}>{entry.name}</span>
          ),
        )}
      </div>
    </div>
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

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
