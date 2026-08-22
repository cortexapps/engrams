/** Retry a review that runs on the automation engine (ADR 0119 phase 4.4).
 *
 * A legacy retry mints a fresh ingress epoch; a built-in retry admits a fresh
 * run of the PR-review built-in with the SAME trigger the original run
 * carried (the review row's `automation_run_id` links to it). A new
 * `retry:<uuid>` delivery key gives it its own durable execution — supersede
 * concurrency on the PR url then stops any pass still in flight for that PR.
 */

import { PR_REVIEW_BUILTIN_KEY } from "../automations/builtins/pr-review.ts";
import {
  admitAutomationRun,
  automationRunId,
  defaultWorkflowStarter,
  type AutomationWebhookStarter,
} from "../automations/dispatch.ts";
import { defaultAutomationSender, type AutomationSender } from "../automations/engine/inbox.ts";
import {
  effectiveDefinition,
  makeAutomationStore,
  type AutomationDispatchStore,
  type AutomationStore,
} from "../db/automations.ts";

export interface RetryAutomationDeps {
  store?: Pick<AutomationStore, "getByBuiltinKey" | "getRun"> & AutomationDispatchStore;
  starter?: AutomationWebhookStarter;
  sender?: AutomationSender;
  now?: () => Date;
  randomUUID?: () => string;
}

export class RetryAutomationError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "RetryAutomationError";
  }
}

/** Admit a fresh built-in run mirroring the review's original trigger.
 * Returns the new run id. Throws if the review never ran on the engine, or
 * the built-in is not seeded. */
export async function retryAutomationReview(
  automationRunId_: string,
  deps: RetryAutomationDeps = {},
): Promise<string> {
  const store = deps.store ?? makeAutomationStore();
  const starter = deps.starter ?? defaultWorkflowStarter();
  const sender = deps.sender ?? defaultAutomationSender;
  const now = deps.now ?? (() => new Date());
  const randomUUID = deps.randomUUID ?? (() => crypto.randomUUID());

  const original = await store.getRun(automationRunId_);
  if (!original) {
    throw new RetryAutomationError("the review's automation run no longer exists");
  }
  const builtin = await store.getByBuiltinKey(PR_REVIEW_BUILTIN_KEY);
  if (!builtin) {
    throw new RetryAutomationError("the PR-review built-in is not seeded");
  }

  const deliveryKey = `retry:${randomUUID()}`;
  const runId = automationRunId(builtin.id, deliveryKey);
  await admitAutomationRun(
    {
      target: {
        automation: builtin,
        definition: effectiveDefinition(builtin.version, builtin.blockOverrides),
      },
      runId,
      deliveryKey,
      trigger: {
        ...original.trigger,
        // A retry is a fresh receipt; keep the original payload/eventKey so the
        // built-in resolves the same PR.
        receivedAt: now().toISOString(),
      },
      scheduledFor: null,
    },
    { store, starter, sender, now },
  );
  return runId;
}
