import { useEffect, useMemo, useState } from "react";
import {
  AssistantRuntimeProvider,
  useExternalStoreRuntime,
  type AppendMessage,
  type ThreadMessageLike,
} from "@assistant-ui/react";
import { Thread } from "@/components/assistant-ui/thread";
import { TooltipProvider } from "@/components/ui/tooltip";
import { useMutation } from "@connectrpc/connect-query";
import {
  sendPrompt as sendPromptMethod,
  interrupt as interruptMethod,
} from "../../gen/engram/app/v1/session-SessionService_connectquery";
import { buildMessages, INACTIVE_STATUSES } from "./buildMessages";
import { SessionStatusContext } from "./session-status";
import type { IndexedEvent, SessionState } from "../../lib/types";

// The transcript tab, on assistant-ui. The session's SSE event stream is the
// single source of truth: `buildMessages` reduces it to the assistant-ui
// message model and we hand that to an external-store runtime, which only
// RENDERS — it never owns the request lifecycle. Sending a prompt is
// fire-and-forget (`sendPrompt`); the echoed user turn and the assistant's
// reply arrive back over the same SSE feed and re-render through `messages`.
//
// No `onEdit`/`onReload` are wired, so assistant-ui's edit/branch tree is
// inert — a server-authoritative log doesn't branch. `onCancel` maps to the
// operator interrupt (the composer's stop control while a run is in flight).

/** Pull the plain-text body out of a composer AppendMessage. */
function appendText(message: AppendMessage): string {
  return message.content
    .filter((p): p is { type: "text"; text: string } => p.type === "text")
    .map((p) => p.text)
    .join("\n")
    .trim();
}

export interface SessionThreadProps {
  sessionId: string;
  events: IndexedEvent[];
  /** Drives whether the composer can send (terminal states block it). */
  status: SessionState | undefined;
}

const SEND_BLOCKED: ReadonlySet<SessionState> = new Set<SessionState>([
  "completed",
  "failed",
  "dead",
  "host_lost",
]);

export function SessionThread({ sessionId, events, status }: SessionThreadProps) {
  const { messages: serverMessages, isRunning } = useMemo(
    () => buildMessages(events, sessionId, status),
    [events, sessionId, status],
  );

  // Phase 1b: optimistic prompts. A prompt the user just submitted is held
  // here (keyed by its client-minted prompt_id) and rendered as a greyed
  // "pending" bubble, so the message is NEVER lost in the window between
  // pressing Enter and the server's authoritative `role:user` echo landing
  // over SSE. When that echo (same prompt_id) arrives, the optimistic entry
  // is pruned and `buildMessages` renders the echo with the SAME id — so it
  // transitions in place (no duplicate, no flicker), staying greyed until
  // its run_started{prompt_id} consumes it.
  const [pending, setPending] = useState<{ promptId: string; text: string }[]>([]);

  const echoedPromptIds = useMemo(() => {
    const s = new Set<string>();
    for (const { event } of events) {
      if (event.type === "agent_message" && event.role === "user" && event.prompt_id) {
        s.add(event.prompt_id);
      }
    }
    return s;
  }, [events]);

  // Drop optimistic entries the server has now echoed (the authoritative
  // message took over rendering).
  useEffect(() => {
    setPending((p) => p.filter((e) => !echoedPromptIds.has(e.promptId)));
  }, [echoedPromptIds]);

  const messages = useMemo(() => {
    const optimistic: ThreadMessageLike[] = pending
      .filter((e) => !echoedPromptIds.has(e.promptId))
      .map((e) => ({
        role: "user",
        id: e.promptId,
        content: [{ type: "text", text: e.text }],
        metadata: { custom: { pending: true } },
      }));
    return optimistic.length ? [...serverMessages, ...optimistic] : serverMessages;
  }, [serverMessages, pending, echoedPromptIds]);

  // ADR 0051 Task 24: sendPrompt + interrupt move to the connect-query
  // useMutation so they flow via the gated passthrough (/rpc/…) rather than
  // the legacy coordinator REST layer. The old fire-and-forget semantics are
  // preserved: we await the mutation promise but don't optimistically mutate
  // any query cache (the SSE stream is the source of truth for runs/events;
  // there's no query to invalidate here).
  const sendPromptMutation = useMutation(sendPromptMethod);
  const interruptMutation = useMutation(interruptMethod);

  const runtime = useExternalStoreRuntime({
    messages,
    isRunning,
    isSendDisabled: status ? SEND_BLOCKED.has(status) : false,
    convertMessage: (m: ThreadMessageLike) => m,
    onNew: async (message) => {
      const text = appendText(message);
      if (!text) return;
      // Mint the prompt_id client-side so the optimistic bubble and the
      // server echo share an identity (dedup + in-place transition).
      const promptId = crypto.randomUUID();
      setPending((p) => [...p, { promptId, text }]);
      try {
        await sendPromptMutation.mutateAsync({ sessionId, text, promptId });
      } catch (err) {
        // Send failed: drop the optimistic bubble so it isn't stuck greyed.
        setPending((p) => p.filter((e) => e.promptId !== promptId));
        console.warn("sendPrompt failed", err);
      }
    },
    onCancel: async () => {
      // Nothing to interrupt once the session is idle/terminal (e.g. it was
      // idle-evicted mid-run) — the interrupt endpoint would 409 on the
      // already-unbound sandbox. No-op so the Stop control is honestly inert.
      if (status && INACTIVE_STATUSES.has(status)) return;
      // The run_interrupted event arrives over SSE and closes the run. A
      // 409 (no live sandbox) is still benign — the run may have just ended.
      try {
        await interruptMutation.mutateAsync({ sessionId });
      } catch (err) {
        console.warn("interrupt failed", err);
      }
    },
  });

  return (
    <AssistantRuntimeProvider runtime={runtime}>
      <SessionStatusContext.Provider value={status}>
        <TooltipProvider>
          <Thread />
        </TooltipProvider>
      </SessionStatusContext.Provider>
    </AssistantRuntimeProvider>
  );
}
