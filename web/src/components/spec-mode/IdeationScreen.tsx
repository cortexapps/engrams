import { useEffect, useMemo, useState } from "react";
import type { Awareness } from "y-protocols/awareness";

import { useAuth } from "@/auth/AuthProvider";
import { Button } from "@/components/ui/button";
import { Text } from "@/components/ui/text";
import { useSendSpecMessage } from "@/hooks/useSpecMessages";
import { collaboratorColor } from "./collaborator-colors";
import { readSpecPresence, type SpecPresenceEntry } from "./section-presence";
import { SpecComposer } from "./SpecComposer";
import { SpecThread } from "./SpecThread";
import { useSpecConversation } from "./useSpecConversation";
import "./spec-mode.css";

export function IdeationScreen({
  specId,
  title,
  templateName,
  awareness,
  isStartingDrafting,
  startDraftingError,
  onStartDrafting,
}: {
  specId: string;
  title: string;
  templateName: string;
  awareness?: Awareness;
  isStartingDrafting: boolean;
  startDraftingError: string | null;
  onStartDrafting: () => void;
}) {
  const conversation = useSpecConversation(specId);
  const sendMessage = useSendSpecMessage(specId);
  const hasFinding = conversation.entries.some(
    (entry) => entry.kind === "agent" && entry.text.trim().length > 0 && entry.citations.length > 0,
  );

  return (
    <main className="spec-mode-ideation-viewport" aria-label="Spec ideation">
      <div className="spec-mode-ideation" style={{ maxWidth: 700 }}>
        <header className="spec-mode-ideation-header">
          <div className="spec-mode-ideation-context">
            <Text as="span" variant="label" tone="muted">
              Thinking it through
            </Text>
            <Text as="span" variant="code" tone="muted">
              {templateName}
            </Text>
          </div>
          <Text as="h1" variant="display" className="spec-mode-ideation-title">
            {title}
          </Text>
          <IdeationPresence awareness={awareness} />
        </header>

        <SpecThread
          entries={conversation.entries}
          isRunning={conversation.isRunning}
          toolLabel={conversation.toolLabel}
          onActivity={() => undefined}
          emptyState={
            <div className="spec-mode-ideation-empty">
              <Text as="h2" variant="heading">
                Start with what you need to work through
              </Text>
              <Text tone="muted">
                Share a question or constraint. Engram will check each claim against the repository
                as the conversation develops.
              </Text>
            </div>
          }
        />

        <footer className="spec-mode-ideation-footer">
          {conversation.hasError ? (
            <Text className="spec-mode-conversation-warning" tone="destructive" role="status">
              Conversation updates are reconnecting.
            </Text>
          ) : null}
          <SpecComposer
            acknowledgedPromptIds={conversation.acknowledgedPromptIds}
            onSend={(message) => sendMessage.mutateAsync(message)}
            placeholder="Think out loud. I'll check each claim against the repo as you go."
          />
          <div className="spec-mode-ideation-bridge">
            <Button type="button" size="sm" disabled={isStartingDrafting} onClick={onStartDrafting}>
              {isStartingDrafting ? "Starting drafting…" : "Start drafting"}
            </Button>
            <Text tone="muted" className="spec-mode-ideation-bridge-note">
              {hasFinding
                ? "The conversation has findings ready to shape the document."
                : "Nothing is written down yet."}
            </Text>
          </div>
          {startDraftingError ? (
            <Text
              as="div"
              className="spec-mode-ideation-start-error"
              tone="destructive"
              role="alert"
            >
              {startDraftingError}
            </Text>
          ) : null}
        </footer>
      </div>
    </main>
  );
}

function IdeationPresence({ awareness }: { awareness?: Awareness }) {
  const { principal } = useAuth();
  const [revision, setRevision] = useState(0);
  const name = principal.display_name || principal.email;
  const localUser = useMemo(
    () => ({ id: principal.email, name, color: collaboratorColor(principal.email) }),
    [name, principal.email],
  );

  useEffect(() => {
    if (!awareness) return;
    const update = () => setRevision((value) => value + 1);
    awareness.setLocalStateField("user", localUser);
    awareness.on("change", update);
    return () => {
      awareness.off("change", update);
      awareness.setLocalStateField("user", null);
    };
  }, [awareness, localUser]);

  const entries = useMemo(
    () => visiblePresence(awareness ? readSpecPresence(awareness) : [], localUser),
    [awareness, localUser, revision],
  );

  return (
    <div className="spec-mode-ideation-presence" aria-label={`${entries.length} people present`}>
      {entries.map((entry) => (
        <span className="spec-mode-ideation-person" key={entry.key}>
          <span
            className="spec-mode-ideation-avatar"
            style={{ borderBottomColor: entry.color }}
            aria-hidden="true"
          >
            {initials(entry.name)}
          </span>
          <Text as="span" tone="muted">
            {entry.isLocal ? "you" : entry.name}
          </Text>
        </span>
      ))}
    </div>
  );
}

interface VisiblePresence {
  key: string;
  name: string;
  color: string;
  isLocal: boolean;
}

function visiblePresence(
  entries: readonly SpecPresenceEntry[],
  localUser: { id: string; name: string; color: string },
): VisiblePresence[] {
  const people = new Map<string, VisiblePresence>();
  people.set(localUser.id, {
    key: localUser.id,
    name: localUser.name,
    color: localUser.color,
    isLocal: true,
  });
  for (const entry of entries) {
    if (entry.kind !== "human") continue;
    const key = entry.id ?? `${entry.clientId}:${entry.name}`;
    people.set(key, {
      key,
      name: entry.name,
      color: entry.color,
      isLocal: entry.id === localUser.id,
    });
  }
  return [...people.values()];
}

function initials(name: string): string {
  return (
    name
      .split(/\s+/)
      .filter(Boolean)
      .slice(0, 2)
      .map((part) => part[0]?.toUpperCase())
      .join("") || "?"
  );
}
