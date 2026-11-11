/** Reviews reached from outside a webhook (ADR 0119 phase 4.4).
 *
 * The forge's webhook admits its code-review built-in on its own. Three
 * other doors into a review exist, and each one admits a built-in run the
 * same way:
 *
 * - **Retry** (`/reviews` → Retry): admits a fresh run with the SAME trigger
 *   the original run carried (the review row's `automation_run_id` links to
 *   it). A new `retry:<uuid>` delivery key gives it its own durable
 *   execution — supersede concurrency on the PR url then stops any pass
 *   still in flight for that PR.
 * - **CI dispatch** (`POST /api/v1/reviews/dispatch`): carries only a
 *   coordinate. It admits a run under the synthetic `review.dispatch` event
 *   key with a minimal payload; the built-in's admission arm accepts it for
 *   any mapped repo (an explicit dispatch is a request, like a comment
 *   command), and `open_review_pass` resolves the heads from the forge because
 *   the payload carries no SHAs. The payload does carry the canonical PR
 *   url, because the built-in's supersede concurrency key is that url: a
 *   dispatch must supersede the same PR's in-flight pass exactly as a
 *   webhook delivery would.
 * - **`@engrams stop`**: the built-in's admission prelude (its `facts` +
 *   `admit` blocks, evaluated by dispatch BEFORE the concurrency claim —
 *   see engine/admission.ts) filters the stop comment out, so its own
 *   delivery never supersedes the live pass. The route then stops that run
 *   directly through the engine inbox — the run ends `halted` and the
 *   `report_halt` finalize hook posts the legacy halt comment.
 */

import { REVIEW_DISPATCH_EVENT_KEY } from "../automations/builtins/review-variant.ts";
import type { ReviewVariant } from "../automations/builtins/review-variant.ts";
import { reviewVariantFor } from "../forges/registry.ts";
import {
  admitAutomationRun,
  automationRunId,
  defaultInstanceStoreLazy,
  defaultWorkflowStarter,
  type AutomationWebhookStarter,
} from "../automations/dispatch.ts";
import { MAIN_ENTRYPOINT_ID, type AutomationDefinition } from "../automations/engine/definition.ts";
import { resolveInstance } from "../automations/instances.ts";
import type { AutomationInstanceStore } from "../db/automation-instances.ts";
import type { AutomationRunTrigger } from "../db/schema.ts";
import { defaultAutomationSender, inboxKeys, type AutomationSender } from "../automations/engine/inbox.ts";
import {
  effectiveDefinition,
  makeAutomationStore,
  type AutomationDispatchStore,
  type AutomationRow,
  type AutomationRunRow,
  type AutomationStore,
} from "../db/automations.ts";
import { makeReviewStore, type ReviewStore } from "../db/reviews.ts";
import { requireForge } from "../forges/registry.ts";
import { log as rootLog } from "../log.ts";

const log = rootLog.child({ component: "automation-review" });

export type AutomationReviewInstanceStore = Pick<
  AutomationInstanceStore,
  "resolveHandles" | "getOpenInstanceByKey" | "getInstance" | "openInstance"
>;

export interface AutomationReviewDeps {
  store?: Pick<AutomationStore, "getByBuiltinKey" | "getRun"> & AutomationDispatchStore;
  starter?: AutomationWebhookStarter;
  sender?: AutomationSender;
  /** ADR 0120: the PR's workstream (the built-in is instanced per PR). */
  instances?: AutomationReviewInstanceStore;
  now?: () => Date;
  randomUUID?: () => string;
}

export class RetryAutomationError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "RetryAutomationError";
  }
}

function resolveDeps(deps: AutomationReviewDeps) {
  return {
    store: deps.store ?? makeAutomationStore(),
    starter: deps.starter ?? defaultWorkflowStarter(),
    sender: deps.sender ?? defaultAutomationSender,
    instances: deps.instances ?? defaultInstanceStoreLazy(),
    now: deps.now ?? (() => new Date()),
    randomUUID: deps.randomUUID ?? (() => crypto.randomUUID()),
  };
}

/** Bind an out-of-webhook admission to the PR's workstream the way dispatch
 * does: resolve by the key template (no facet here, so no handle tier),
 * join the open workstream or open one. A non-instanced definition binds
 * to nothing (`''`). */
async function bindWorkstream(
  builtin: AutomationRow,
  definition: AutomationDefinition,
  trigger: AutomationRunTrigger,
  deliveryKey: string,
  instances: AutomationReviewInstanceStore,
): Promise<{ instanceId: string; inputs?: Record<string, unknown> }> {
  const resolution = await resolveInstance(
    {
      target: { automation: builtin, definition },
      entrypoint: { id: MAIN_ENTRYPOINT_ID, trigger: definition.trigger },
      trigger,
    },
    { instances },
  );
  switch (resolution.kind) {
    case "none":
      return { instanceId: "" };
    case "bound":
      return { instanceId: resolution.instance.id, inputs: resolution.instance.inputs };
    case "open": {
      const opened = await instances.openInstance({
        automationId: builtin.id,
        key: resolution.key,
        inputs: resolution.inputs,
        openedBy: `review:${deliveryKey}`,
      });
      return { instanceId: opened.id, inputs: opened.inputs };
    }
    case "drop":
      throw new RetryAutomationError(`the review does not route to a workstream (${resolution.reason})`);
  }
}

async function requireBuiltin(
  store: Pick<AutomationStore, "getByBuiltinKey">,
  provider: string,
): Promise<{ builtin: AutomationRow; variant: ReviewVariant }> {
  const variant = reviewVariantFor(provider);
  const builtin = variant === null ? null : await store.getByBuiltinKey(variant.key);
  if (variant === null || !builtin) {
    throw new RetryAutomationError(`the code-review built-in for "${provider}" is not seeded`);
  }
  return { builtin, variant };
}

/** Admit a fresh run of the forge's built-in with the review's original
 * trigger. Returns the new run id. Throws if the original run is gone or
 * the built-in is not seeded. */
export async function retryAutomationReview(
  automationRunId_: string,
  provider: string,
  deps: AutomationReviewDeps = {},
): Promise<string> {
  const { store, starter, sender, instances, now, randomUUID } = resolveDeps(deps);

  const original = await store.getRun(automationRunId_);
  if (!original) {
    throw new RetryAutomationError("the review's automation run no longer exists");
  }
  // The built-in whose enrolment the caller checked.
  const { builtin } = await requireBuiltin(store, provider);
  // The trigger of another automation's run means nothing to the built-in.
  if (original.automationId !== builtin.id) {
    throw new RetryAutomationError(
      "another automation wrote this review; start the review again from that automation",
    );
  }
  const definition = effectiveDefinition(builtin.version, builtin.blockOverrides);

  const deliveryKey = `retry:${randomUUID()}`;
  const trigger: AutomationRunTrigger = {
    ...original.trigger,
    // A retry is a fresh receipt; keep the original payload/eventKey so the
    // built-in resolves the same PR.
    receivedAt: now().toISOString(),
  };
  const bound = await bindWorkstream(builtin, definition, trigger, deliveryKey, instances);
  const runId = automationRunId(builtin.id, deliveryKey, MAIN_ENTRYPOINT_ID, bound.instanceId);
  await admitAutomationRun(
    {
      target: { automation: builtin, definition },
      runId,
      deliveryKey,
      ...(bound.instanceId !== "" ? { instanceId: bound.instanceId, instanceInputs: bound.inputs } : {}),
      trigger,
      scheduledFor: null,
    },
    { store, starter, sender, now },
  );
  return runId;
}

export interface ReviewCoordinate {
  /** The forge's connector provider. */
  provider: string;
  repo: string;
  prNumber: number;
}

/** Admit a built-in run for a PR named only by coordinate (the CI dispatch
 * edge). Returns the new run id, which is also the DBOS workflow id. */
export async function dispatchAutomationReview(
  input: ReviewCoordinate,
  deps: AutomationReviewDeps = {},
): Promise<string> {
  const { store, starter, sender, instances, now, randomUUID } = resolveDeps(deps);
  const { builtin, variant } = await requireBuiltin(store, input.provider);
  const definition = effectiveDefinition(builtin.version, builtin.blockOverrides);

  const deliveryKey = `dispatch:${randomUUID()}`;
  const trigger: AutomationRunTrigger = {
    source: "manual",
    eventKey: REVIEW_DISPATCH_EVENT_KEY,
    receivedAt: now().toISOString(),
    scopeValue: input.repo,
    payload: variant.dispatchPayload({ repo: input.repo, prNumber: input.prNumber }),
  };
  const bound = await bindWorkstream(builtin, definition, trigger, deliveryKey, instances);
  const runId = automationRunId(builtin.id, deliveryKey, MAIN_ENTRYPOINT_ID, bound.instanceId);
  await admitAutomationRun(
    {
      target: { automation: builtin, definition },
      runId,
      deliveryKey,
      ...(bound.instanceId !== "" ? { instanceId: bound.instanceId, instanceInputs: bound.inputs } : {}),
      trigger,
      scheduledFor: null,
    },
    { store, starter, sender, now },
  );
  return runId;
}

/** Run statuses a stop can still reach (mirrors `AutomationService.StopRun`). */
const STOPPABLE: ReadonlySet<string> = new Set(["pending", "waiting", "running"]);

export interface StopAutomationReviewDeps {
  reviews?: Pick<ReviewStore, "getActiveReviewByCoordinate">;
  runs?: Pick<AutomationStore, "getRun">;
  sender?: AutomationSender;
  randomUUID?: () => string;
}

export type StopAutomationReviewResult =
  | { stopped: true; runId: string }
  | { stopped: false; reason: "no_active_review" | "not_on_engine" | "run_not_live" };

/** Stop the built-in run driving a PR's active review pass. The active pass
 * is the review row still in flight for the coordinate; its
 * `automation_run_id` names the run. Nothing to stop is a result, never an
 * error — the legacy `@stop` was equally quiet on an idle PR. */
export async function stopAutomationReview(
  input: ReviewCoordinate,
  deps: StopAutomationReviewDeps = {},
): Promise<StopAutomationReviewResult> {
  const reviews = deps.reviews ?? makeReviewStore();
  const runs = deps.runs ?? makeAutomationStore();
  const sender = deps.sender ?? defaultAutomationSender;
  const randomUUID = deps.randomUUID ?? (() => crypto.randomUUID());

  const active = await reviews.getActiveReviewByCoordinate(input.provider, input.repo, input.prNumber);
  if (!active) return { stopped: false, reason: "no_active_review" };
  if (!active.automationRunId) return { stopped: false, reason: "not_on_engine" };
  const run: AutomationRunRow | null = await runs.getRun(active.automationRunId);
  if (!run || !STOPPABLE.has(run.status)) return { stopped: false, reason: "run_not_live" };

  await sender.send(
    run.id,
    { kind: "stop", reason: "stopped by @mention command" },
    inboxKeys.stop(run.id, randomUUID()),
  );
  log.info({ ...input, runId: run.id }, "stop command halted the built-in review run");
  return { stopped: true, runId: run.id };
}
