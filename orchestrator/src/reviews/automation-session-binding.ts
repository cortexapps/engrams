/** ReviewSessionBinding over `automation_session` (ADR 0119 phase 4.2).
 *
 * When the built-in automation drives a review, its worker sessions bind to
 * the RUN (not a review workflow id): the automation consumer and the
 * `signal_automation`/dual-path review tools route `finder_done` and
 * `verifier_done` to `autorun:<runId>` through this row. `keep: false` so
 * the run's finalize ends any straggler the explicit end_session blocks
 * missed. The row is not removed on worker teardown: finalize reads it, and
 * a stale binding for an ended session routes nothing.
 */

import { makeAutomationEngineStore } from "../db/automations.ts";
import type { ReviewSessionBinding } from "./control-plane.ts";

export interface AutomationSessionBindingStore {
  recordSessionBinding(input: {
    sessionId: string;
    runId: string;
    blockId: string;
    role: string;
    keep: boolean;
  }): Promise<void>;
}

export function makeAutomationReviewSessionBinding(
  runId: string,
  blockId: string,
  store: AutomationSessionBindingStore = makeAutomationEngineStore(),
): ReviewSessionBinding {
  return {
    async record(sessionId, role) {
      await store.recordSessionBinding({ sessionId, runId, blockId, role, keep: false });
    },
    async remove() {
      // Intentionally a no-op — see the module doc.
    },
  };
}
