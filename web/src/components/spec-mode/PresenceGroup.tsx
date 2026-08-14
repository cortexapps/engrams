import { Text } from "@/components/ui/text";
import type { SpecPresenceEntry } from "./section-presence";
import type { SpecSurfaceSection } from "./spec-surface";

export function PresenceGroup({
  presence,
  sections,
}: {
  presence: SpecPresenceEntry[];
  sections: SpecSurfaceSection[];
}) {
  if (presence.length === 0) return null;

  return (
    <div className="spec-mode-presence-group" aria-label={`${presence.length} present`}>
      {presence.map((entry) => {
        const key =
          entry.kind === "agent"
            ? `agent-${entry.clientId}-${entry.toolCallId}`
            : `human-${entry.clientId}`;
        if (entry.kind === "agent") {
          const section = sections.find((candidate) => candidate.id === entry.sectionId);
          const label = section ? `${entry.name} · §${section.title}` : entry.name;
          return (
            <span key={key} className="spec-mode-presence-pair" data-presence-kind="agent">
              <span className="spec-mode-presence-avatar is-agent" aria-hidden="true">
                ✦
              </span>
              <Text as="span" variant="code" className="spec-mode-presence-label">
                {label}
              </Text>
            </span>
          );
        }
        return (
          <span key={key} className="spec-mode-presence-pair" data-presence-kind="human">
            <span
              className="spec-mode-presence-avatar"
              style={{ backgroundColor: entry.color }}
              aria-hidden="true"
            >
              {initials(entry.name)}
            </span>
            <Text as="span" variant="code" className="spec-mode-presence-label">
              {entry.isSelf ? "you" : entry.name}
            </Text>
          </span>
        );
      })}
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
