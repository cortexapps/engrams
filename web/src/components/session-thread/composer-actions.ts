import { createContext, useContext } from "react";

/**
 * ADR 0052: the composer's actions, provided by `SessionThread` (which owns the
 * mutations + the queue) and consumed by the `Composer` deep in the assistant-ui
 * tree (`thread.tsx`).
 *
 * We drive submit/interrupt OURSELVES rather than through assistant-ui's
 * `ComposerPrimitive.Send`/`Cancel`, which are run-gated (they block submit
 * while a run is in flight). Bypassing them lets a prompt be ENQUEUED
 * (type-ahead) mid-run, and gives the Send⇄Stop button, ⌘↵, and Esc one shared
 * source of truth.
 */
export interface ComposerActions {
  /** Submit `text`. Idle → starts a run; mid-run → the harness QUEUES it
   *  (type-ahead). No-op on blank text; the caller clears the composer. */
  submit: (text: string) => void;
  /** Interrupt the in-flight run (Esc / the Stop button). No-op when idle. */
  interrupt: () => void;
  /** Terminal session (completed/failed/dead/host_lost) — Send is disabled. */
  sendBlocked: boolean;
  /** Is there a still-queued prompt the user can recall (↑)? */
  canRecall: boolean;
  /**
   * Pull the most-recent still-queued prompt OUT of the queue (fires
   * `DequeueQueued`) and return its text to load into the composer for
   * editing/cancelling. `null` if the queue is empty.
   */
  recall: () => string | null;
}

export const ComposerActionsContext = createContext<ComposerActions>({
  submit: () => {},
  interrupt: () => {},
  sendBlocked: false,
  canRecall: false,
  recall: () => null,
});

export const useComposerActions = (): ComposerActions => useContext(ComposerActionsContext);
