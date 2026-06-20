import { useCallback, useEffect, useMemo, useRef, useState } from "react";
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
  dequeueQueuedPrompt as dequeueQueuedPromptMethod,
  answerQuestion as answerQuestionMethod,
} from "../../gen/engram/app/v1/session-SessionService_connectquery";
import { buildMessages, INACTIVE_STATUSES } from "./buildMessages";
import { SessionStatusContext } from "./session-status";
import { ComposerActionsContext } from "./composer-actions";
import { QuestionActionsContext } from "./question-actions";
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
  /**
   * Phase 1c: the live token tail (ephemeral `agent_message_chunk` deltas),
   * from `useSessionEvents`. Rendered into the in-flight assistant turn so
   * tokens stream as they arrive; superseded by the durable `agent_message`.
   */
  streamingText?: string;
}

const SEND_BLOCKED: ReadonlySet<SessionState> = new Set<SessionState>([
  "completed",
  "failed",
  "dead",
  "host_lost",
]);

export function SessionThread({
  sessionId,
  events,
  status,
  streamingText = "",
}: SessionThreadProps) {
  const {
    messages: serverMessages,
    isRunning,
    queue,
  } = useMemo(
    () => buildMessages(events, sessionId, status, streamingText),
    [events, sessionId, status, streamingText],
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

  // ADR 0052: full prompt text by prompt_id, retained for the session so the
  // ↑-recall can repopulate the composer with the COMPLETE text even after the
  // optimistic `pending` entry is pruned on echo (the wire `prompt_queued`
  // summary is truncated to ~1 KB). Bounded — one short entry per prompt sent.
  const sentTextRef = useRef(new Map<string, string>());

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
  const dequeueQueuedMutation = useMutation(dequeueQueuedPromptMethod);
  const answerQuestionMutation = useMutation(answerQuestionMethod);

  // ADR 0054: optimistic answered-question state. A submitted answer shows its
  // receipt immediately (greyed) — the session resumes and the authoritative
  // `question_answered` event round-trips over SSE seconds later. On a send
  // failure we drop the id so the card returns to its form (selections kept).
  const [answeredToolCallIds, setAnsweredToolCallIds] = useState<Set<string>>(new Set());

  const sendBlocked = status ? SEND_BLOCKED.has(status) : false;

  const submitAnswer = useCallback(
    (toolCallId: string, answers: Record<string, string[]>) => {
      if (sendBlocked) return;
      setAnsweredToolCallIds((prev) => new Set(prev).add(toolCallId));
      // proto3 maps can't hold a `repeated` value, so each answer is wrapped in
      // a StringList (the init shape is a plain `{ values }`).
      const answersInit: Record<string, { values: string[] }> = {};
      for (const [question, labels] of Object.entries(answers)) {
        answersInit[question] = { values: labels };
      }
      answerQuestionMutation
        .mutateAsync({ sessionId, toolCallId, answers: answersInit })
        .catch((err) => {
          setAnsweredToolCallIds((prev) => {
            const next = new Set(prev);
            next.delete(toolCallId);
            return next;
          });
          console.warn("answerQuestion failed", err);
        });
    },
    [sessionId, sendBlocked, answerQuestionMutation],
  );

  // ADR 0052: ↑-in-empty-composer recall. Pull the most-recent still-queued
  // prompt OUT of the queue (DequeueQueued, so it can't be claimed mid-edit)
  // and hand its text back to the composer. Full text from `sentTextRef` when
  // this client sent it; otherwise the (possibly-truncated) wire summary.
  const recallQueued = useCallback((): string | null => {
    if (queue.length === 0) return null;
    const last = queue[queue.length - 1]!;
    const text = sentTextRef.current.get(last.promptId) ?? last.summary;
    setPending((p) => p.filter((e) => e.promptId !== last.promptId));
    dequeueQueuedMutation
      .mutateAsync({ sessionId, promptId: last.promptId })
      .catch((err) => console.warn("dequeueQueuedPrompt failed", err));
    return text;
  }, [queue, sessionId, dequeueQueuedMutation]);

  // ADR 0052: submit a prompt. Idle → starts a run; mid-run → the harness
  // QUEUES it (type-ahead). We drive this ourselves (not assistant-ui's
  // run-gated send) so the composer can enqueue while a run is in flight. The
  // optimistic bubble (keyed by the client-minted prompt_id) covers the gap
  // until the server's role:user echo lands with the same id.
  const submit = useCallback(
    (raw: string) => {
      const text = raw.trim();
      if (!text) return;
      const promptId = crypto.randomUUID();
      setPending((p) => [...p, { promptId, text }]);
      sentTextRef.current.set(promptId, text);
      sendPromptMutation.mutateAsync({ sessionId, text, promptId }).catch((err) => {
        // Send failed: drop the optimistic bubble so it isn't stuck greyed.
        setPending((p) => p.filter((e) => e.promptId !== promptId));
        console.warn("sendPrompt failed", err);
      });
    },
    [sessionId, sendPromptMutation],
  );

  // ADR 0052/0030: interrupt the in-flight run (Esc / the Stop button). No-op
  // once idle/terminal — the endpoint would 409 on the unbound sandbox. The
  // run_interrupted event arrives over SSE and closes the run; a queued
  // message (if any) then runs next per the harness's consume-on-result.
  const interrupt = useCallback(() => {
    if (status && INACTIVE_STATUSES.has(status)) return;
    interruptMutation
      .mutateAsync({ sessionId })
      .catch((err) => console.warn("interrupt failed", err));
  }, [sessionId, status, interruptMutation]);

  const runtime = useExternalStoreRuntime({
    messages,
    isRunning,
    isSendDisabled: sendBlocked,
    convertMessage: (m: ThreadMessageLike) => m,
    // The composer drives submit/interrupt through ComposerActionsContext (so
    // it can enqueue mid-run); these adapters keep any assistant-ui-internal
    // submit/cancel path consistent with our own.
    onNew: async (message) => submit(appendText(message)),
    onCancel: async () => interrupt(),
  });

  return (
    <AssistantRuntimeProvider runtime={runtime}>
      <SessionStatusContext.Provider value={status}>
        <ComposerActionsContext.Provider
          value={{
            submit,
            interrupt,
            sendBlocked,
            canRecall: queue.length > 0,
            recall: recallQueued,
          }}
        >
          <QuestionActionsContext.Provider
            value={{ submitAnswer, answeredToolCallIds, sendBlocked }}
          >
            <TooltipProvider>
              <Thread />
            </TooltipProvider>
          </QuestionActionsContext.Provider>
        </ComposerActionsContext.Provider>
      </SessionStatusContext.Provider>
    </AssistantRuntimeProvider>
  );
}
