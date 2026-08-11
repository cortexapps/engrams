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
  completeToolCall as completeToolCallMethod,
} from "../../gen/engram/app/v1/session-SessionService_connectquery";
import { buildMessages } from "./buildMessages";
import { SessionStatusContext } from "./session-status";
import { ComposerActionsContext, type InterruptSource } from "./composer-actions";
import {
  NO_TRANSCRIPT_BACKFILL,
  TranscriptWindowContext,
  type TranscriptWindow,
} from "./transcript-window";
import { QuestionActionsContext } from "./question-actions";
import type { IndexedEvent, SessionState } from "../../lib/types";
import { serializeComposer, useSessionUploads } from "../session-files/useSessionUploads";
import { SessionFileContext } from "../session-files/UploadPathText";

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
  /**
   * The windowed transcript's backfill controls, also from `useSessionEvents`.
   * Omitted = the caller holds the whole log and there is nothing to read.
   */
  transcriptWindow?: TranscriptWindow;
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
  transcriptWindow,
}: SessionThreadProps) {
  const uploads = useSessionUploads(sessionId);
  const {
    messages: serverMessages,
    isRunning,
    queue,
    pendingPlan,
    currentMode,
  } = useMemo(
    () =>
      buildMessages(
        events,
        sessionId,
        status,
        streamingText,
        // Below the window floor the spine carries a deferred question without
        // the result that answered it, so an old decision must read as unknown
        // rather than as one still waiting on the reviewer.
        transcriptWindow?.hasMore ? (transcriptWindow.oldestIdx ?? null) : null,
      ),
    [
      events,
      sessionId,
      status,
      streamingText,
      transcriptWindow?.hasMore,
      transcriptWindow?.oldestIdx,
    ],
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
  //
  // Each entry is tagged with the `sessionId` it was sent to. `SessionThread` is
  // intentionally NOT remounted on a session switch (the sessions rail navigates
  // by path param only), so this single `pending` array is shared across every
  // session the user visits in one page load — the `sessionId` tag is what keeps
  // an optimistic bubble pinned to its origin session instead of bleeding into
  // every other session's transcript. (Surviving navigation away-and-back is the
  // point; it's reset only on a full page load.)
  const [pending, setPending] = useState<
    { sessionId: string; promptId: string; text: string; queued: boolean }[]
  >([]);

  // ADR 0052: full prompt text by prompt_id, retained for the session so the
  // ↑-recall / rail can show the COMPLETE text even after the optimistic
  // `pending` entry is pruned (the wire `prompt_queued` summary is truncated to
  // ~1 KB). Bounded — one short entry per prompt sent.
  const sentTextRef = useRef(new Map<string, string>());

  // prompt_ids the server has CONSUMED (`run_started{prompt_id}` or
  // `prompt_steered{prompt_id}` — the bubble is now in the durable transcript)
  // or DEQUEUED (recalled / cancelled). Either
  // ends an optimistic entry's life.
  const consumedPromptIds = useMemo(() => {
    const s = new Set<string>();
    for (const { event } of events) {
      if (event.type === "run_started" && event.prompt_id) s.add(event.prompt_id);
      if (event.type === "prompt_steered") s.add(event.prompt_id);
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
  // ADR 0108 (held-echo UX): prompt_ids whose durable user echo has landed.
  // buildMessages now renders an unconsumed, unqueued echo as a pending bubble
  // keyed by its prompt_id — the optimistic bubble (same id) must yield to it,
  // or assistant-ui sees a duplicate message id.
  const echoedPromptIds = useMemo(() => {
    const s = new Set<string>();
    for (const { event } of events) {
      if (event.type === "agent_message" && event.role === "user" && event.prompt_id)
        s.add(event.prompt_id);
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
        (e) =>
          e.sessionId === sessionId &&
          !e.queued &&
          !consumedPromptIds.has(e.promptId) &&
          !queuedPromptIds.has(e.promptId) &&
          !echoedPromptIds.has(e.promptId),
      )
      .map((e) => ({
        role: "user",
        id: e.promptId,
        content: [{ type: "text", text: e.text }],
        metadata: { custom: { pending: true } },
      }));
    return optimistic.length ? [...serverMessages, ...optimistic] : serverMessages;
  }, [serverMessages, pending, consumedPromptIds, queuedPromptIds, echoedPromptIds, sessionId]);

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
        e.sessionId !== sessionId ||
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
  }, [queue, pending, consumedPromptIds, dequeuedPromptIds, sessionId]);

  // ADR 0051 Task 24: sendPrompt + interrupt move to the connect-query
  // useMutation so they flow via the gated passthrough (/rpc/…) rather than
  // the legacy coordinator REST layer. The old fire-and-forget semantics are
  // preserved: we await the mutation promise but don't optimistically mutate
  // any query cache (the SSE stream is the source of truth for runs/events;
  // there's no query to invalidate here).
  const sendPromptMutation = useMutation(sendPromptMethod);
  const interruptMutation = useMutation(interruptMethod);
  const dequeueQueuedMutation = useMutation(dequeueQueuedPromptMethod);
  const completeToolCallMutation = useMutation(completeToolCallMethod);

  // Optimistic answered-question state. CompleteToolCall appends
  // tool_result_submitted synchronously, but the browser can still observe the
  // mutation response before SSE delivery. On failure the card returns to its form.
  const [answeredToolCallIds, setAnsweredToolCallIds] = useState<Set<string>>(new Set());

  const sessionSendBlocked = status ? SEND_BLOCKED.has(status) : false;
  const sendBlocked = sessionSendBlocked || !uploads.ready;

  // ADR 0107: the generic deferred-tool completion — questions and plan
  // decisions ride the same CompleteToolCall + optimistic set.
  const completeTool = useCallback(
    (toolCallId: string, result: unknown) => {
      if (sendBlocked) return;
      setAnsweredToolCallIds((prev) => new Set(prev).add(toolCallId));
      const completion = completeToolCallMutation.mutateAsync({
        sessionId,
        toolCallId,
        resultJson: JSON.stringify(result),
      });
      completion.catch((err) => {
        setAnsweredToolCallIds((prev) => {
          const next = new Set(prev);
          next.delete(toolCallId);
          return next;
        });
        console.warn("completeToolCall failed", err);
      });
    },
    [sessionId, sendBlocked, completeToolCallMutation],
  );

  const submitAnswer = useCallback(
    (toolCallId: string, answers: Record<string, string[]>) => completeTool(toolCallId, answers),
    [completeTool],
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
  // ADR 0107: the composer's mode. The server truth is `currentMode`
  // (derived from the event log); `modeOverride` is the user's not-yet-sent
  // toggle. The next prompt carries `harnessMode` only when it CHANGES the
  // mode, so a steady state never spams mode markers.
  const [modeOverride, setModeOverride] = useState<string | null>(null);
  const composerMode = modeOverride ?? currentMode;
  const setMode = useCallback(
    (next: string) => setModeOverride(next === currentMode ? null : next),
    [currentMode],
  );

  const submit = useCallback(
    (raw: string) => {
      const text = serializeComposer(raw, uploads.tokens);
      if (!text) return;
      const promptId = crypto.randomUUID();
      // `queued` is the client's optimistic guess (was a run in flight when we
      // sent?) — it routes the bubble to the rail vs. inline until the server's
      // prompt_queued / run_started confirms which it is.
      setPending((p) => [...p, { sessionId, promptId, text, queued: isRunning }]);
      sentTextRef.current.set(promptId, text);
      const harnessMode =
        modeOverride !== null && modeOverride !== currentMode ? modeOverride : undefined;
      setModeOverride(null);
      sendPromptMutation
        .mutateAsync({ sessionId, text, promptId, ...(harnessMode ? { harnessMode } : {}) })
        .then(() => uploads.clear())
        .catch((err) => {
          // Send failed: drop the optimistic entry so it isn't stuck.
          setPending((p) => p.filter((e) => e.promptId !== promptId));
          console.warn("sendPrompt failed", err);
        });
    },
    [sessionId, sendPromptMutation, isRunning, modeOverride, currentMode, uploads],
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

  // ADR 0052/0030/0108: interrupt the in-flight run (Esc / the Stop button /
  // the assistant-ui cancel adapter). Two gates, both required, so a phantom
  // interrupt cannot fire from a page that is not visibly running:
  //   1. client run state — the thread must believe a run is live (the same
  //      `isRunning` the composer's Stop/Esc affordances key on);
  //   2. session status — only states where a live run can exist. Never
  //      `undefined` (page load) and never host_lost/idle/terminal (the
  //      endpoint would 409 on the unbound sandbox anyway).
  // `source` attributes the caller on InterruptRequest.source. The
  // run_interrupted event arrives over SSE and closes the run; a queued
  // message (if any) then runs next per the harness's consume-on-result.
  const interrupt = useCallback(
    (source: InterruptSource) => {
      if (!isRunning) {
        console.debug("interrupt suppressed: no run is live", { source, status });
        return;
      }
      if (status !== "active" && status !== "created" && status !== "evicting") {
        console.debug("interrupt suppressed: session status has no live run", { source, status });
        return;
      }
      interruptMutation
        .mutateAsync({ sessionId, source })
        .catch((err) => console.warn("interrupt failed", err));
    },
    [sessionId, status, isRunning, interruptMutation],
  );

  const runtime = useExternalStoreRuntime({
    messages,
    isRunning,
    isSendDisabled: sendBlocked,
    convertMessage: (m: ThreadMessageLike) => m,
    // The composer drives submit/interrupt through ComposerActionsContext (so
    // it can enqueue mid-run); these adapters keep any assistant-ui-internal
    // submit/cancel path consistent with our own.
    onNew: async (message) => submit(appendText(message)),
    // Still reachable through the runtime's cancel API with cancelOnEscape
    // off; gated inside `interrupt` like every other caller (ADR 0108).
    onCancel: async () => interrupt("aui-cancel"),
  });

  return (
    <SessionFileContext.Provider value={sessionId}>
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
              mode: composerMode,
              setMode,
              planPending: pendingPlan != null,
              uploads: uploads.tokens,
              addFiles: uploads.addFiles,
              addCanonicalPath: uploads.addCanonicalPath,
              removeUpload: uploads.remove,
              retryUpload: uploads.retry,
            }}
          >
            <QuestionActionsContext.Provider
              value={{ submitAnswer, completeTool, answeredToolCallIds, sendBlocked }}
            >
              <TranscriptWindowContext.Provider value={transcriptWindow ?? NO_TRANSCRIPT_BACKFILL}>
                <TooltipProvider>
                  <Thread />
                </TooltipProvider>
              </TranscriptWindowContext.Provider>
            </QuestionActionsContext.Provider>
          </ComposerActionsContext.Provider>
        </SessionStatusContext.Provider>
      </AssistantRuntimeProvider>
    </SessionFileContext.Provider>
  );
}
