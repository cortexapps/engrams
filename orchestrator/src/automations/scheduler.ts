/** Lease-claimed dynamic cron scanner (ADR 0102). */

import { DBOS } from "@dbos-inc/dbos-sdk";
import { Cron } from "croner";
import { hostname } from "node:os";

import { makeAutomationStore, type AutomationCronStore, type DueCronAutomation } from "../db/automations.ts";
import { log as rootLog } from "../log.ts";
import { automationRunWorkflow, type AutomationRunWorkflowInput } from "../workflows/automation-run.ts";

const log = rootLog.child({ component: "automation-scheduler" });

export const AUTOMATION_SCAN_INTERVAL_MS = 15_000;
export const AUTOMATION_LEASE_TTL_MS = 2 * 60_000;
export const AUTOMATION_MISSED_FIRE_GRACE_MS = 10 * 60_000;

export interface AutomationWorkflowStarter {
  start(input: AutomationRunWorkflowInput, workflowId: string): Promise<void>;
}

export interface AutomationSchedulerTickDeps {
  owner: string;
  store: AutomationCronStore;
  workflowStarter: AutomationWorkflowStarter;
  now: () => Date;
  onError?: (automation: DueCronAutomation, error: unknown) => void;
}

export interface AutomationSchedulerTickResult {
  due: number;
  claimed: number;
  started: number;
  skipped: number;
  errors: number;
}

export function automationCronWorkflowId(
  automationId: string,
  scheduledFor: Date,
): string {
  return `auto:${automationId}:${Math.floor(scheduledFor.getTime() / 1_000)}`;
}

export function nextCronOccurrence(
  automation: Pick<DueCronAutomation, "trigger">,
  after: Date,
): Date {
  const cron = new Cron(automation.trigger.schedule, {
    timezone: automation.trigger.timezone,
    paused: true,
  });
  const next = cron.nextRun(after);
  if (!next) throw new Error("cron schedule has no future occurrence");
  return next;
}

/** One independently driveable scanner step. Every database decision remains
 * in AutomationCronStore; this function only sequences claim, durable start,
 * and the post-start schedule CAS. */
export async function runSchedulerTick(
  deps: AutomationSchedulerTickDeps,
): Promise<AutomationSchedulerTickResult> {
  const now = deps.now();
  const due = await deps.store.listDueCron(now);
  const result: AutomationSchedulerTickResult = {
    due: due.length,
    claimed: 0,
    started: 0,
    skipped: 0,
    errors: 0,
  };

  for (const automation of due) {
    try {
      const scheduledFor = automation.nextFireAt;
      const claim = await deps.store.claimCronOccurrence({
        automationId: automation.id,
        scheduledFor,
        leaseOwner: deps.owner,
        leaseExpiresAt: new Date(now.getTime() + AUTOMATION_LEASE_TTL_MS),
        now,
      });
      if (claim === null) continue;
      if (claim.kind === "claimed") result.claimed++;

      const nextFireAt = nextCronOccurrence(automation, now);
      const workflowId = automationCronWorkflowId(automation.id, scheduledFor);
      const workflowInput: AutomationRunWorkflowInput = {
        automationId: automation.id,
        runId: claim.run.id,
        trigger: { source: "cron" },
        scheduledFor: scheduledFor.toISOString(),
        // Keep retries of this occurrence byte-for-byte stable.
        receivedAt: scheduledFor.toISOString(),
      };

      if (claim.kind === "terminal") {
        if (claim.run.status !== "skipped") {
          // This is the start-before-advance crash window. Starting the same
          // terminal DBOS id is a no-op and never creates a successor epoch.
          await deps.workflowStarter.start(workflowInput, workflowId);
          result.started++;
        }
        await deps.store.advanceCronSchedule({
          automationId: automation.id,
          scheduledFor,
          nextFireAt,
          fired: claim.run.status !== "skipped",
          now,
        });
        continue;
      }

      const latenessMs = now.getTime() - scheduledFor.getTime();
      if (latenessMs > AUTOMATION_MISSED_FIRE_GRACE_MS) {
        await deps.store.markRunSkipped(
          claim.run.id,
          `cron occurrence missed by ${latenessMs}ms (grace ${AUTOMATION_MISSED_FIRE_GRACE_MS}ms)`,
        );
        await deps.store.advanceCronSchedule({
          automationId: automation.id,
          scheduledFor,
          nextFireAt,
          fired: false,
          now,
        });
        result.skipped++;
        continue;
      }

      await deps.workflowStarter.start(workflowInput, workflowId);
      result.started++;
      await deps.store.advanceCronSchedule({
        automationId: automation.id,
        scheduledFor,
        nextFireAt,
        fired: true,
        now,
      });
    } catch (error) {
      result.errors++;
      deps.onError?.(automation, error);
    }
  }
  return result;
}

export interface AutomationSchedulerDeps extends AutomationSchedulerTickDeps {
  setInterval?: (fn: () => void, ms: number) => ReturnType<typeof setInterval>;
  clearInterval?: (timer: ReturnType<typeof setInterval>) => void;
}

export class AutomationScheduler {
  readonly #deps: AutomationSchedulerDeps;
  #timer: ReturnType<typeof setInterval> | null = null;
  #tick: Promise<AutomationSchedulerTickResult> | null = null;

  constructor(deps: AutomationSchedulerDeps) {
    this.#deps = deps;
  }

  runOnce(): Promise<AutomationSchedulerTickResult> {
    this.#tick ??= runSchedulerTick(this.#deps).finally(() => {
      this.#tick = null;
    });
    return this.#tick;
  }

  async start(): Promise<void> {
    if (this.#timer !== null) return;
    await this.runOnce();
    const schedule = this.#deps.setInterval ?? setInterval;
    this.#timer = schedule(() => void this.runOnce(), AUTOMATION_SCAN_INTERVAL_MS);
  }

  async stop(): Promise<void> {
    if (this.#timer !== null) {
      const cancel = this.#deps.clearInterval ?? clearInterval;
      cancel(this.#timer);
      this.#timer = null;
    }
    await this.#tick;
  }
}

export function makeProductionAutomationScheduler(): AutomationScheduler {
  const owner = `${hostname()}:${process.pid}:${crypto.randomUUID()}`;
  return new AutomationScheduler({
    owner,
    store: makeAutomationStore(),
    now: () => new Date(),
    workflowStarter: {
      async start(input, workflowId) {
        await DBOS.startWorkflow(automationRunWorkflow, { workflowID: workflowId })(input);
      },
    },
    onError(automation, error) {
      log.error({ automationId: automation.id, error }, "automation scheduler tick failed");
    },
  });
}
