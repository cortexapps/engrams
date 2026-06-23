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

  // Phase 1b: optimistic prompts. A prompt the user just submitted is held here
  // (keyed by its client-minted prompt_id) so it's NEVER lost in the window
  // between pressing Enter and the server's authoritative echo landing over SSE.
  // It renders two ways, by `queued`:
  //   - idle send (`queued:false`) → an inline user bubble in the thread, which
  //     the durable `role:user` echo (same prompt_id) replaces in place (no
  //     duplicate, no flicker) once it arrives.
  //   - mid-run send (`queued:true`) → the composer's queued-message rail
  //     (Claude-Code style), NOT the thread; `buildMessages` only drops it into
  //     the transcript at its `run_started{prompt_id}` consumption point, so it
  //     lands in true conversation order (below the turn it was queued behind).
  // Either way the entry is pruned once the run starts (consumed) or the message
  // is dequeued.
  const [pending, setPending] = useState<{ promptId: string; text: string; queued: boolean }[]>([]);

  // ADR 0052: full prompt text by prompt_id, retained for the session so the
  // ↑-recall / rail can show the COMPLETE text even after the optimistic
  // `pending` entry is pruned (the wire `prompt_queued` summary is truncated to
  // ~1 KB). Bounded — one short entry per prompt sent.
  const sentTextRef = useRef(new Map<string, string>());

  // prompt_ids the server has CONSUMED (`run_started{prompt_id}` — the bubble is
  // now in the durable transcript) or DEQUEUED (recalled / cancelled). Either
  // ends an optimistic entry's life.
  const consumedPromptIds = useMemo(() => {
    const s = new Set<string>();
    for (const { event } of events) {
      if (event.type === "run_started" && event.prompt_id) s.add(event.prompt_id);
    }
    return s;
  }, [events]);
  const dequeuedPromptIds = useMemo(() => {
    const s = new Set<string>();
    for (const { event } of events) {
      if (event.type === "prompt_dequeued") s.add(event.prompt_id);
    }
    return s;
  }, [events]);
  // Server-confirmed queue membership (from `prompt_queued`, surfaced via
  // buildMessages' `queue`). An optimistic "immediate" send the server actually
  // queued (a run started just as it landed) moves to the rail once this knows.
  const queuedPromptIds = useMemo(() => new Set(queue.map((q) => q.promptId)), [queue]);

  // Drop optimistic entries the server has consumed (the durable bubble took
  // over) or dequeued (recalled / cancelled).
  useEffect(() => {
    setPending((p) =>
      p.filter((e) => !consumedPromptIds.has(e.promptId) && !dequeuedPromptIds.has(e.promptId)),
    );
  }, [consumedPromptIds, dequeuedPromptIds]);

  // Transcript = serverMessages + the IMMEDIATE optimistic bubbles (idle sends
  // not yet consumed and not re-classified as queued by the server). Queued
  // messages are NOT in the thread — they live in the composer rail.
  const messages = useMemo(() => {
    const optimistic: ThreadMessageLike[] = pending
      .filter(
        (e) => !e.queued && !consumedPromptIds.has(e.promptId) && !queuedPromptIds.has(e.promptId),
      )
      .map((e) => ({
        role: "user",
        id: e.promptId,
        content: [{ type: "text", text: e.text }],
        metadata: { custom: { pending: true } },
      }));
    return optimistic.length ? [...serverMessages, ...optimistic] : serverMessages;
  }, [serverMessages, pending, consumedPromptIds, queuedPromptIds]);

  // The composer's queued-message rail (Claude-Code style): everything submitted
  // but not yet consumed into the conversation. Server-confirmed queue first
  // (oldest→newest), then any just-submitted optimistic ones the server hasn't
  // acked. Deduped by prompt_id; full text preferred over the wire summary.
  const railItems = useMemo(() => {
    const seen = new Set<string>();
    const items: { promptId: string; text: string }[] = [];
    for (const q of queue) {
      if (seen.has(q.promptId)) continue;
      seen.add(q.promptId);
      items.push({ promptId: q.promptId, text: sentTextRef.current.get(q.promptId) ?? q.summary });
    }
    for (const e of pending) {
      if (
        !e.queued ||
        seen.has(e.promptId) ||
        consumedPromptIds.has(e.promptId) ||
        dequeuedPromptIds.has(e.promptId)
      )
        continue;
      seen.add(e.promptId);
      items.push({ promptId: e.promptId, text: e.text });
    }
    return items;
  }, [queue, pending, consumedPromptIds, dequeuedPromptIds]);

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
      // `queued` is the client's optimistic guess (was a run in flight when we
      // sent?) — it routes the bubble to the rail vs. inline until the server's
      // prompt_queued / run_started confirms which it is.
      setPending((p) => [...p, { promptId, text, queued: isRunning }]);
      sentTextRef.current.set(promptId, text);
      sendPromptMutation.mutateAsync({ sessionId, text, promptId }).catch((err) => {
        // Send failed: drop the optimistic entry so it isn't stuck.
        setPending((p) => p.filter((e) => e.promptId !== promptId));
        console.warn("sendPrompt failed", err);
      });
    },
    [sessionId, sendPromptMutation, isRunning],
  );

  // Cancel a specific queued message (the rail's × button): dequeue it server-
  // side and drop any optimistic entry. Distinct from `recall` (↑), which pulls
  // the newest queued message back into the composer for editing.
  const removeQueued = useCallback(
    (promptId: string) => {
      setPending((p) => p.filter((e) => e.promptId !== promptId));
      dequeueQueuedMutation
        .mutateAsync({ sessionId, promptId })
        .catch((err) => console.warn("dequeueQueuedPrompt failed", err));
    },
    [sessionId, dequeueQueuedMutation],
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
            queued: railItems,
            removeQueued,
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
