import { useState } from "react";

import { Button } from "@/components/ui/button";
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
}: {
  next: NextProposal;
  onSend: (message: string) => Promise<unknown>;
}) {
  const [pendingPrompt, setPendingPrompt] = useState<string | null>(null);
  const [failedPrompt, setFailedPrompt] = useState<string | null>(null);
  const content = proposalContent(next);
  if (!content) return null;

  const send = async (prompt: string) => {
    setPendingPrompt(prompt);
    setFailedPrompt(null);
    try {
      await onSend(prompt);
    } catch {
      setFailedPrompt(prompt);
    } finally {
      setPendingPrompt(null);
    }
  };

  return (
    <section className="spec-mode-next" aria-label="Next proposal">
      <Text as="h2" variant="label" tone="muted">
        Next — I propose
      </Text>
      <Text className="spec-mode-next-copy">{content.description}</Text>
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
          {content.secondaryLabel}
        </Button>
      </div>
      {pendingPrompt ? (
        <Text as="div" tone="muted" role="status">
          Sending next step…
        </Text>
      ) : null}
      {failedPrompt ? (
        <div className="spec-mode-next-error" role="alert">
          <Text tone="destructive">Next step not sent.</Text>
          <Button type="button" size="xs" variant="outline" onClick={() => void send(failedPrompt)}>
            Retry
          </Button>
        </div>
      ) : null}
    </section>
  );
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
