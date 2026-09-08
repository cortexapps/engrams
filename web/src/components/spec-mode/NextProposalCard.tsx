import { useState } from "react";

import { Button } from "@/components/ui/button";
import { EmptyState } from "@/components/empty-state";
import { Text } from "@/components/ui/text";
import type { NextProposal } from "./spec-surface";

interface ProposalContent {
  description: string;
  primaryLabel: string;
  primaryPrompt: string;
  secondaryLabel: string;
  secondaryPrompt: string;
}

export function NextProposalCard({
  next,
  onSend,
  alternateLabel,
}: {
  next: NextProposal;
  onSend: (message: string) => Promise<unknown>;
  alternateLabel?: string;
}) {
  const [pendingPrompt, setPendingPrompt] = useState<string | null>(null);
  const [failedPrompt, setFailedPrompt] = useState<string | null>(null);
  // Which proposal we have already accepted. A proposal is derived from server
  // state that only moves once the agent has done the work, so without this the
  // card keeps offering an instruction that is already in flight — and a second
  // click enqueues it twice.
  const [acceptedKey, setAcceptedKey] = useState<string | null>(null);
  const content = proposalContent(next);
  if (!content) return null;
  const key = proposalKey(next);
  const accepted = acceptedKey === key;

  const send = async (prompt: string) => {
    setPendingPrompt(prompt);
    setFailedPrompt(null);
    try {
      await onSend(prompt);
      setAcceptedKey(key);
    } catch {
      setFailedPrompt(prompt);
    } finally {
      setPendingPrompt(null);
    }
  };

  return (
    <section className="spec-mode-next" aria-label="Next proposal">
      <h2 className="text-sm font-semibold">Next — I propose</h2>
      <Text className="spec-mode-next-copy">{content.description}</Text>
      {accepted ? null : (
        <div className="spec-mode-next-actions">
          <Button
            type="button"
            size="sm"
            disabled={pendingPrompt !== null}
            onClick={() => void send(content.primaryPrompt)}
          >
            {content.primaryLabel}
          </Button>
          <Button
            type="button"
            size="sm"
            variant="outline"
            disabled={pendingPrompt !== null}
            onClick={() => void send(content.secondaryPrompt)}
          >
            {alternateLabel ?? content.secondaryLabel}
          </Button>
        </div>
      )}
      {pendingPrompt ? (
        <Text as="div" tone="muted" role="status">
          Sending next step…
        </Text>
      ) : null}
      {accepted && !pendingPrompt ? (
        <Text as="div" tone="muted" role="status">
          Asked. Waiting for the agent to pick it up.
        </Text>
      ) : null}
      {failedPrompt ? (
        <EmptyState
          inline
          tone="error"
          className="spec-mode-next-error"
          action={
            <Button
              type="button"
              size="xs"
              variant="outline"
              onClick={() => void send(failedPrompt)}
            >
              Retry
            </Button>
          }
        >
          Next step not sent.
        </EmptyState>
      ) : null}
    </section>
  );
}

// Identifies a proposal so an accepted one can be told from the next one. Two
// proposals that read the same ask for the same work.
export function proposalKey(next: NextProposal): string {
  if (!next) return "none";
  if (next.kind === "draft_section" || next.kind === "settle_section") {
    return `${next.kind}:${next.sectionId}`;
  }
  return next.kind;
}

export function proposalContent(next: NextProposal): ProposalContent | null {
  if (!next) return null;
  if (next.kind === "draft_section") {
    return {
      description: `Draft §${next.sectionTitle} from the evidence already in the conversation, then check it against the document.`,
      primaryLabel: "Do it",
      primaryPrompt: `Draft §${next.sectionTitle} now. Use the repository evidence already in the conversation.`,
      secondaryLabel: "Something else",
      secondaryPrompt: `I want to choose a different next step instead of §${next.sectionTitle}.`,
    };
  }
  if (next.kind === "settle_section") {
    return {
      description: `Review §${next.sectionTitle} with me, then decide whether it is ready to settle.`,
      primaryLabel: "Walk me through it",
      primaryPrompt: `Walk me through §${next.sectionTitle} before we decide whether to settle it.`,
      secondaryLabel: "Later",
      secondaryPrompt: `Leave §${next.sectionTitle} as it is for now.`,
    };
  }
  return {
    description:
      "Look for what breaks across the settled sections and record questions in the document.",
    primaryLabel: "Look for what breaks",
    primaryPrompt:
      "Look for what breaks across the spec. Record each unresolved risk as a question in its section.",
    secondaryLabel: "Not yet",
    secondaryPrompt: "Do not look for breakage yet. Wait for another instruction.",
  };
}
