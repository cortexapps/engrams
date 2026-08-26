/** Lease-claimed dynamic cron scanner (ADR 0102, runs on the ADR 0119 engine). */

import { DBOS } from "@dbos-inc/dbos-sdk";
import { Cron } from "croner";
import { hostname } from "node:os";

import { makeAutomationStore, type AutomationCronStore, type DueCronAutomation } from "../db/automations.ts";
import type { AutomationInstanceStore } from "../db/automation-instances.ts";
import { defaultInstanceStoreLazy } from "./dispatch.ts";
import { makeIntegrationEventStore } from "../db/integration-events.ts";
import { log as rootLog } from "../log.ts";
import { automationRunWorkflow, type AutomationRunWorkflowInput } from "../workflows/automation-run.ts";
import { admitClaimedCronRun, automationRunId, cronDeliveryKey } from "./dispatch.ts";
import { defaultAutomationSender, type AutomationSender } from "./engine/inbox.ts";

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
  /** Mailbox sender for join/supersede policies; defaults to DBOS. */
  sender?: AutomationSender;
  /** ADR 0120: the fan-out enumeration for instanced automations. */
  instances?: Pick<AutomationInstanceStore, "listOpenInstances">;
  now: () => Date;
  onError?: (automation: DueCronAutomation, error: unknown) => void;
}

export interface AutomationSchedulerTickResult {
  due: number;
  claimed: number;
  started: number;
  skipped: number;
  /** Occurrences settled by a concurrency policy without starting (join,
   * skip, lost supersede race) or parked as queued. */
  admitted: { joined: number; queued: number; skipped: number };
  errors: number;
}

export function automationCronWorkflowId(
  automationId: string,
  scheduledFor: Date,
  entrypointId?: string,
  instanceId = "",
): string {
  return automationRunId(automationId, cronDeliveryKey(scheduledFor), entrypointId, instanceId);
}

/** ADR 0120: how many open workstreams one cron tick fans out to, per
 * automation. Above the cap the oldest LIMIT run and a warning names the
 * rest (no silent truncation). */
export const CRON_FANOUT_MAX_INSTANCES = 500;

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
 * in AutomationCronStore; this function sequences claim, concurrency
 * admission, durable start, and the post-start schedule CAS.
 *
 * The occurrence claim is exclusive per (automation, scheduled_for); the
 * concurrency policy (queue | supersede | skip | join) is then applied to
 * the claimed run exactly as the dispatchers apply it — one admission
 * semantics for every trigger kind. */
export async function runSchedulerTick(
  deps: AutomationSchedulerTickDeps,
): Promise<AutomationSchedulerTickResult> {
  const now = deps.now();
  const sender = deps.sender ?? defaultAutomationSender;
  const due = await deps.store.listDueCron(now);
  const result: AutomationSchedulerTickResult = {
    due: due.length,
    claimed: 0,
    started: 0,
    skipped: 0,
    admitted: { joined: 0, queued: 0, skipped: 0 },
    errors: 0,
  };

  const instances = deps.instances ?? defaultInstanceStoreLazy();

  /** Claim + admit + start ONE occurrence row (per instance for an
   * instanced automation; the classic single row otherwise). Returns
   * whether the occurrence FIRED (anything but a filtered/missed row) —
   * the advance-once aggregation reads it. */
  async function processOccurrence(
    dueAutomation: DueCronAutomation,
    scheduledFor: Date,
    instanceId: string,
  ): Promise<boolean | null> {
    const automation = dueAutomation.automation;
    const workflowId = automationCronWorkflowId(
      automation.id,
      scheduledFor,
      dueAutomation.entrypointId,
      instanceId,
    );
    const claim = await deps.store.claimCronOccurrence({
      runId: workflowId,
      automationId: automation.id,
      version: automation.currentVersion,
      entrypointId: dueAutomation.entrypointId,
      ...(instanceId !== "" ? { instanceId } : {}),
      scheduledFor,
      leaseOwner: deps.owner,
      leaseExpiresAt: new Date(now.getTime() + AUTOMATION_LEASE_TTL_MS),
      now,
    });
    // Another pod holds a live lease on this row: IT advances the schedule.
    if (claim === null) return null;
    if (claim.kind === "claimed") result.claimed++;

    const workflowInput: AutomationRunWorkflowInput = {
      runId: claim.run.id,
      automationId: automation.id,
    };

    if (claim.kind === "terminal") {
      if (claim.run.status !== "filtered") {
        // This is the start-before-advance crash window. Starting the same
        // terminal DBOS id is a no-op and never creates a successor epoch.
        await deps.workflowStarter.start(workflowInput, claim.run.id);
        result.started++;
        return true;
      }
      return false;
    }
    // Durably started and still running: the occurrence fired; nothing to
    // start, and the advance must NOT wait for the run to finish (a
    // long-running sibling would otherwise freeze the whole cadence).
    if (claim.kind === "in_flight") return true;

    const latenessMs = now.getTime() - scheduledFor.getTime();
    if (latenessMs > AUTOMATION_MISSED_FIRE_GRACE_MS) {
      await deps.store.markRunSkipped(
        claim.run.id,
        `cron occurrence missed by ${latenessMs}ms (grace ${AUTOMATION_MISSED_FIRE_GRACE_MS}ms)`,
      );
      result.skipped++;
      return false;
    }

    // Concurrency admission on the claimed row (debt ledger 3.1). A
    // settled/queued occurrence still fired: the occurrence happened, the
    // policy decided what to do with it.
    const admission = await admitClaimedCronRun(
      { target: { automation, definition: dueAutomation.definition }, run: claim.run, trigger: claim.run.trigger },
      { store: deps.store, sender, now: deps.now },
    );
    if (admission !== "started") {
      result.admitted[admission]++;
      return true;
    }

    await deps.workflowStarter.start(workflowInput, claim.run.id);
    result.started++;
    return true;
  }

  for (const dueAutomation of due) {
    const automation = dueAutomation.automation;
    try {
      const scheduledFor = dueAutomation.nextFireAt;
      const nextFireAt = nextCronOccurrence(dueAutomation, now);

      // ADR 0120 fan-out: an instanced automation admits ONE occurrence per
      // OPEN workstream (each with its own durable workflow id, occurrence
      // row, and instance-scoped run); 0 open workstreams = a quiet tick.
      // The schedule advances ONCE after the loop — a mid-loop crash leaves
      // next_fire_at unchanged, and every already-claimed sibling
      // reconverges idempotently on the retry tick. A workstream closed
      // between list and claim still runs (admitted while open; runs never
      // re-check).
      let fired = false;
      let anyHeld = false;
      if (dueAutomation.definition.settings.instance !== undefined) {
        const open = await instances.listOpenInstances(
          automation.id,
          CRON_FANOUT_MAX_INSTANCES,
        );
        if (open.length === CRON_FANOUT_MAX_INSTANCES) {
          log.warn(
            { automationId: automation.id, cap: CRON_FANOUT_MAX_INSTANCES },
            "cron fan-out hit the open-workstream cap; older workstreams ran, newer ones did not",
          );
        }
        for (const instance of open) {
          const outcome = await processOccurrence(dueAutomation, scheduledFor, instance.id);
          if (outcome === null) anyHeld = true;
          if (outcome === true) fired = true;
        }
      } else {
        const outcome = await processOccurrence(dueAutomation, scheduledFor, "");
        if (outcome === null) anyHeld = true;
        if (outcome === true) fired = true;
      }
      // ANY pending row lease-held by another pod defers the advance to a
      // later tick: advancing past scheduledFor while a sibling's claimer
      // might die pre-start would strand that occurrence forever (the
      // advance CAS has no instance dimension, and listDueCron reads only
      // the current next_fire_at). A held row resolves within one lease
      // TTL — its holder STARTS it (the row leaves pending and classifies
      // in_flight, which never defers), or the lease expires and a later
      // tick reacquires it — and THAT tick advances. Same invariant as the
      // single-row path: the schedule moves only when no occurrence sits
      // between claim and durable start on another pod.
      if (anyHeld) continue;

      await deps.store.advanceCronSchedule({
        automationId: automation.id,
        scheduledFor,
        nextFireAt,
        fired,
        now,
      });
    } catch (error) {
      result.errors++;
      deps.onError?.(dueAutomation, error);
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

// ---------------------------------------------------------------------------
// Integration-event ledger sweep (ADR 0119 D5)
// ---------------------------------------------------------------------------

export const INTEGRATION_EVENT_SWEEP_INTERVAL_MS = 3600_000;

export interface IntegrationEventSweeperDeps {
  sweep(now: Date): Promise<number>;
  now: () => Date;
  onError?: (error: unknown) => void;
  setInterval?: (fn: () => void, ms: number) => ReturnType<typeof setInterval>;
  clearInterval?: (timer: ReturnType<typeof setInterval>) => void;
}

/** Hourly 7-day-retention sweep over integration_event. A plain timer with
 * the spawn/run_once split — idempotent deletes, so no lease: concurrent pods
 * racing the same sweep both succeed. */
export class IntegrationEventSweeper {
  readonly #deps: IntegrationEventSweeperDeps;
  #timer: ReturnType<typeof setInterval> | null = null;
  #run: Promise<number> | null = null;

  constructor(deps: IntegrationEventSweeperDeps) {
    this.#deps = deps;
  }

  runOnce(): Promise<number> {
    this.#run ??= this.#deps
      .sweep(this.#deps.now())
      .catch((error) => {
        this.#deps.onError?.(error);
        return 0;
      })
      .finally(() => {
        this.#run = null;
      });
    return this.#run;
  }

  async start(): Promise<void> {
    if (this.#timer !== null) return;
    await this.runOnce();
    const schedule = this.#deps.setInterval ?? setInterval;
    this.#timer = schedule(() => void this.runOnce(), INTEGRATION_EVENT_SWEEP_INTERVAL_MS);
  }

  async stop(): Promise<void> {
    if (this.#timer !== null) {
      const cancel = this.#deps.clearInterval ?? clearInterval;
      cancel(this.#timer);
      this.#timer = null;
    }
    await this.#run;
  }
}

export function makeProductionIntegrationEventSweeper(): IntegrationEventSweeper {
  return new IntegrationEventSweeper({
    sweep: (now) => makeIntegrationEventStore().sweep(now),
    now: () => new Date(),
    onError(error) {
      log.error({ error }, "integration event sweep failed");
    },
  });
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
      log.error({ automationId: automation.automation.id, error }, "automation scheduler tick failed");
    },
  });
}
