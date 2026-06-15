import { useMemo } from "react";
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
  const { messages, isRunning } = useMemo(
    () => buildMessages(events, sessionId, status),
    [events, sessionId, status],
  );

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
      if (text) await sendPromptMutation.mutateAsync({ sessionId, text });
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
