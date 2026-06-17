import { createContext, useContext } from "react";

/**
 * ADR 0052: the composer's queued-message recall affordance.
 *
 * `SessionThread` owns the queue (reduced from session events) and the
 * `DequeueQueued` RPC, but the composer that handles ↑ lives deep in the
 * assistant-ui tree (`thread.tsx`). This context bridges them — mirroring
 * `SessionStatusContext`.
 */
export interface QueuedRecall {
  /** Is there a still-queued prompt the user can recall (↑) / cancel? */
  canRecall: boolean;
  /**
   * Pull the most-recent still-queued prompt OUT of the queue (fires
   * `DequeueQueued`, so it can't be claimed mid-edit) and return its text to
   * load into the composer. `null` if the queue is empty. Re-submitting the
   * composer sends it as a fresh prompt; abandoning it leaves it cancelled.
   */
  recall: () => string | null;
}

export const QueuedRecallContext = createContext<QueuedRecall>({
  canRecall: false,
  recall: () => null,
});

export const useQueuedRecall = (): QueuedRecall => useContext(QueuedRecallContext);
