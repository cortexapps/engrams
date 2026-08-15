import { useLayoutEffect, useRef, type ReactNode, type UIEvent } from "react";

import { Markdown } from "@/components/Markdown";
import { Text } from "@/components/ui/text";
import { collaboratorColor } from "./collaborator-colors";
import type { SpecThreadEntry } from "./buildSpecThread";

const BOTTOM_TOLERANCE_PX = 24;

/** A message the viewer sent that the shared conversation has not echoed yet. */
export interface PendingSpecMessage {
  key: string;
  text: string;
  authorName: string;
}

export function SpecThread({
  entries,
  pending = [],
  isRunning,
  toolLabel,
  onActivity,
  emptyState,
}: {
  entries: readonly SpecThreadEntry[];
  pending?: readonly PendingSpecMessage[];
  isRunning: boolean;
  toolLabel?: string | null;
  onActivity: (sectionId: string) => void;
  emptyState?: ReactNode;
}) {
  const threadRef = useRef<HTMLDivElement | null>(null);
  const followsTail = useRef(true);
  const tailKey = `${entries.at(-1)?.id ?? "empty"}:${entries.length}:${pending.length}:${isRunning}:${toolLabel ?? ""}`;

  useLayoutEffect(() => {
    const thread = threadRef.current;
    if (!thread || !followsTail.current) return;
    if (typeof thread.scrollTo === "function") thread.scrollTo({ top: thread.scrollHeight });
    else thread.scrollTop = thread.scrollHeight;
  }, [tailKey]);

  const trackScroll = (event: UIEvent<HTMLDivElement>) => {
    followsTail.current = isThreadAtBottom(event.currentTarget);
  };

  return (
    <div
      ref={threadRef}
      className="spec-mode-thread"
      data-scroll="thread"
      role="log"
      aria-label="Spec conversation"
      aria-live="polite"
      onScroll={trackScroll}
    >
      {entries.length === 0 && pending.length === 0 && !isRunning ? emptyState : null}
      {entries.map((entry) => {
        if (entry.kind === "document_activity") {
          const sections = entry.sectionTitles.map((title) => `§${title}`).join(", ");
          return (
            <button
              key={entry.id}
              type="button"
              className="spec-mode-activity spec-mode-fadein"
              aria-label={`Go to ${entry.sectionTitles[0] ?? "updated section"}`}
              onClick={() => {
                const sectionId = entry.sectionIds[0];
                if (sectionId) onActivity(sectionId);
              }}
            >
              <span className="spec-mode-activity-mark" aria-hidden="true">
                ✎
              </span>
              <span className="spec-mode-activity-copy">Updated {sections}</span>
              <Time value={entry.createdAt} />
            </button>
          );
        }
        if (entry.kind === "phase_change") {
          return (
            <div key={entry.id} className="spec-mode-phase-chip spec-mode-fadein" role="status">
              <Text as="span" variant="code" tone="muted">
                {entry.requestedBy} started drafting
              </Text>
              <Time value={entry.createdAt} />
            </div>
          );
        }
        if (entry.kind === "human") {
          const name = entry.author?.name.trim() || "Collaborator";
          const identity = entry.author?.id || name;
          return (
            <article key={entry.id} className="spec-mode-human-turn spec-mode-fadein">
              <div className="spec-mode-human-label">
                <Text as="span" variant="label" tone="muted">
                  {name}
                </Text>
                <span
                  className="spec-mode-human-avatar"
                  style={{ borderBottomColor: collaboratorColor(identity) }}
                  aria-hidden="true"
                >
                  {name.charAt(0).toUpperCase() || "?"}
                </span>
              </div>
              <div className="spec-mode-human-bubble spec-mode-markdown">
                {entry.text ? <Markdown text={entry.text} /> : "This message is not available."}
              </div>
              <Time value={entry.createdAt} />
            </article>
          );
        }
        return (
          <article key={entry.id} className="spec-mode-agent-turn spec-mode-fadein">
            <Text as="span" variant="label" tone="muted">
              engram
            </Text>
            <div className="spec-mode-agent-copy spec-mode-markdown">
              <Markdown text={entry.text} />
            </div>
            <Time value={entry.createdAt} />
          </article>
        );
      })}
      {pending.map((message) => (
        <article
          key={message.key}
          className="spec-mode-human-turn spec-mode-pending"
          aria-label="Sending message"
        >
          <div className="spec-mode-human-label">
            <Text as="span" variant="label" tone="muted">
              {message.authorName}
            </Text>
          </div>
          <div className="spec-mode-human-bubble spec-mode-markdown">
            <Markdown text={message.text} />
          </div>
          <Text as="span" variant="code" tone="muted" role="status">
            Sending…
          </Text>
        </article>
      ))}
      {isRunning ? (
        <div className="spec-mode-thinking" role="status">
          <span className="spec-mode-penbeat" aria-hidden="true">
            ●
          </span>
          <Text as="span" tone="muted">
            {toolLabel ?? "Engram is thinking"}
          </Text>
        </div>
      ) : null}
    </div>
  );
}

export function isThreadAtBottom(
  element: Pick<HTMLElement, "clientHeight" | "scrollHeight" | "scrollTop">,
) {
  return element.scrollHeight - element.scrollTop - element.clientHeight <= BOTTOM_TOLERANCE_PX;
}

function Time({ value }: { value: string | null }) {
  if (!value) return null;
  const date = new Date(value);
  if (Number.isNaN(date.valueOf())) return null;
  return (
    <Text as="time" variant="code" tone="muted" dateTime={value}>
      {date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}
    </Text>
  );
}
