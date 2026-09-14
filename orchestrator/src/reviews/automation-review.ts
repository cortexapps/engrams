/** Reviews that run on the automation engine, reached from outside a webhook
 * (ADR 0119 phase 4.4).
 *
 * The GitHub webhook spine admits the PR-review built-in on its own. Three
 * other doors into a review exist, and during the parallel window each one
 * must reach the SAME engine the repo's `engine` flag selects, or a flagged
 * repo gets two brains:
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
 *   command), and `open_review_pass` resolves the heads from GitHub because
 *   the payload carries no SHAs. The payload does carry the canonical PR
 *   url, because the built-in's supersede concurrency key is that url: a
 *   dispatch must supersede the same PR's in-flight pass exactly as a
 *   webhook delivery would.
 * - **`@engrams stop`**: the built-in's admission filters the stop comment
 *   out (it is not a review request), so the route stops the PR's live run
 *   directly through the engine inbox — the run ends `halted` and the
 *   `report_halt` finalize hook posts the legacy halt comment.
 */

import { PR_REVIEW_BUILTIN_KEY, REVIEW_DISPATCH_EVENT_KEY } from "../automations/builtins/pr-review.ts";
import {
  admitAutomationRun,
  automationRunId,
  defaultWorkflowStarter,
  type AutomationWebhookStarter,
} from "../automations/dispatch.ts";
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
import { log as rootLog } from "../log.ts";

const log = rootLog.child({ component: "automation-review" });

export interface AutomationReviewDeps {
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

function resolveDeps(deps: AutomationReviewDeps) {
  return {
    store: deps.store ?? makeAutomationStore(),
    starter: deps.starter ?? defaultWorkflowStarter(),
    sender: deps.sender ?? defaultAutomationSender,
    now: deps.now ?? (() => new Date()),
    randomUUID: deps.randomUUID ?? (() => crypto.randomUUID()),
  };
}

async function requireBuiltin(
  store: Pick<AutomationStore, "getByBuiltinKey">,
): Promise<AutomationRow> {
  const builtin = await store.getByBuiltinKey(PR_REVIEW_BUILTIN_KEY);
  if (!builtin) {
    throw new RetryAutomationError("the PR-review built-in is not seeded");
  }
  return builtin;
}

/** Admit a fresh built-in run mirroring the review's original trigger.
 * Returns the new run id. Throws if the review never ran on the engine, or
 * the built-in is not seeded. */
export async function retryAutomationReview(
  automationRunId_: string,
  deps: AutomationReviewDeps = {},
): Promise<string> {
  const { store, starter, sender, now, randomUUID } = resolveDeps(deps);

  const original = await store.getRun(automationRunId_);
  if (!original) {
    throw new RetryAutomationError("the review's automation run no longer exists");
  }
  const builtin = await requireBuiltin(store);

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

export interface ReviewCoordinate {
  repo: string;
  prNumber: number;
}

/** Admit a built-in run for a PR named only by coordinate (the CI dispatch
 * edge). Returns the new run id, which is also the DBOS workflow id. */
export async function dispatchAutomationReview(
  input: ReviewCoordinate,
  deps: AutomationReviewDeps = {},
): Promise<string> {
  const { store, starter, sender, now, randomUUID } = resolveDeps(deps);
  const builtin = await requireBuiltin(store);

  const deliveryKey = `dispatch:${randomUUID()}`;
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
        source: "manual",
        eventKey: REVIEW_DISPATCH_EVENT_KEY,
        receivedAt: now().toISOString(),
        scopeValue: input.repo,
        payload: {
          repository: { full_name: input.repo, name: input.repo.split("/")[1] ?? "" },
          pull_request: {
            number: input.prNumber,
            html_url: `https://github.com/${input.repo}/pull/${input.prNumber}`,
          },
        },
      },
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

  const active = await reviews.getActiveReviewByCoordinate("github", input.repo, input.prNumber);
  if (!active) return { stopped: false, reason: "no_active_review" };
  if (!active.automationRunId) return { stopped: false, reason: "not_on_engine" };
  const run: AutomationRunRow | null = await runs.getRun(active.automationRunId);
  if (!run || !STOPPABLE.has(run.status)) return { stopped: false, reason: "run_not_live" };

  await sender.send(
    run.id,
    { kind: "stop", reason: "stopped by @mention command" },
    inboxKeys.stop(run.id, randomUUID()),
  );
  log.info({ repo: input.repo, prNumber: input.prNumber, runId: run.id }, "github stop command halted the built-in review run");
  return { stopped: true, runId: run.id };
}
