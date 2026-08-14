import { useEffect, useState, type FormEvent, type KeyboardEvent } from "react";
import { RotateCcwIcon, SendIcon } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Text } from "@/components/ui/text";

interface PendingMessage {
  promptId: string | null;
  text: string;
}

export function SpecComposer({
  acknowledgedPromptIds,
  onSend,
}: {
  acknowledgedPromptIds: ReadonlySet<string>;
  onSend: (message: string) => Promise<{ promptId: string }>;
}) {
  const [text, setText] = useState("");
  const [pending, setPending] = useState<PendingMessage | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!pending?.promptId || !acknowledgedPromptIds.has(pending.promptId)) return;
    setText((current) => (current === pending.text ? "" : current));
    setPending(null);
  }, [acknowledgedPromptIds, pending]);

  const send = async () => {
    const message = text.trim();
    if (!message || pending) return;
    setError(null);
    setPending({ promptId: null, text: message });
    try {
      const result = await onSend(message);
      setPending({ promptId: result.promptId, text: message });
    } catch (cause: unknown) {
      setPending(null);
      setError(
        cause instanceof Error && cause.message
          ? `Message not sent: ${cause.message}`
          : "Message not sent. Your text is still here.",
      );
    }
  };

  const submit = (event: FormEvent) => {
    event.preventDefault();
    void send();
  };

  const keyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key !== "Enter" || event.shiftKey || event.nativeEvent.isComposing) return;
    event.preventDefault();
    void send();
  };

  return (
    <form className="spec-mode-composer" onSubmit={submit}>
      <div className="spec-mode-composer-row">
        <textarea
          rows={2}
          value={text}
          maxLength={20_000}
          aria-label="Message the spec collaborators"
          aria-invalid={error ? true : undefined}
          disabled={pending !== null}
          placeholder="Reply, or select any passage in the document to talk about it"
          onChange={(event) => {
            setText(event.target.value);
            setError(null);
          }}
          onKeyDown={keyDown}
        />
        <Button
          type="submit"
          size="icon-sm"
          aria-label="Send message"
          disabled={pending !== null || text.trim().length === 0}
        >
          <SendIcon />
        </Button>
      </div>
      {pending ? (
        <Text as="div" className="spec-mode-composer-status" tone="muted" role="status">
          {pending.promptId ? "Waiting for the shared conversation…" : "Sending message…"}
        </Text>
      ) : null}
      {error ? (
        <div className="spec-mode-composer-error" role="alert">
          <Text tone="destructive">{error}</Text>
          <Button type="button" size="xs" variant="outline" onClick={() => void send()}>
            <RotateCcwIcon />
            Retry
          </Button>
        </div>
      ) : null}
    </form>
  );
}
