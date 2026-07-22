import { and, asc, desc, eq, isNull, lte, sql } from "drizzle-orm";

import { getDb } from "./client.ts";
import {
  automation as automationTable,
  automationRun as automationRunTable,
  task as taskTable,
  taskSession as taskSessionTable,
  webhookRegistration as webhookRegistrationTable,
  webhookSample as webhookSampleTable,
  type AutomationAction,
  type AutomationRunTrigger,
  type AutomationTrigger,
  type WebhookVerification,
} from "./schema.ts";

export interface AutomationRow {
  id: string;
  name: string;
  description: string;
  enabled: boolean;
  trigger: AutomationTrigger;
  action: AutomationAction;
  createdByUserId: string | null;
  nextFireAt: Date | null;
  lastFiredAt: Date | null;
  createdAt: Date;
  updatedAt: Date;
  archivedAt: Date | null;
}

export interface AutomationInput {
  name: string;
  description: string;
  enabled: boolean;
  trigger: AutomationTrigger;
  action: AutomationAction;
  nextFireAt: Date | null;
}

export interface AutomationRunRow {
  id: string;
  automationId: string;
  trigger: AutomationRunTrigger;
  renderedPrompt: string | null;
  renderedTitle: string | null;
  taskId: string | null;
  sessionId: string | null;
  status: string;
  error: string | null;
  scheduledFor: Date | null;
  leaseOwner: string | null;
  leaseExpiresAt: Date | null;
  createdAt: Date;
}

export interface WebhookRegistrationRow {
  id: string;
  name: string;
  verification: WebhookVerification;
  providerHint: string | null;
  createdByUserId: string | null;
  createdAt: Date;
  updatedAt: Date;
}

export interface WebhookSampleRow {
  id: string;
  registrationId: string;
  eventKey: string;
  payload: Record<string, unknown>;
  receivedAt: Date;
}

export interface AutomationStore {
  list(opts: { includeArchived: boolean }): Promise<AutomationRow[]>;
  get(id: string): Promise<AutomationRow | null>;
  getActive(id: string): Promise<AutomationRow | null>;
  listActiveForWebhookRegistration(registrationId: string): Promise<AutomationRow[]>;
  create(input: AutomationInput, createdByUserId: string): Promise<AutomationRow>;
  update(id: string, input: AutomationInput): Promise<AutomationRow | null>;
  archive(id: string): Promise<AutomationRow | null>;
  setEnabled(
    id: string,
    enabled: boolean,
    nextFireAt?: Date | null,
  ): Promise<AutomationRow | null>;
  listRuns(automationId: string, limit: number): Promise<AutomationRunRow[]>;

  createRegistration(input: {
    id: string;
    name: string;
    verification: WebhookVerification;
    providerHint: string | null;
    createdByUserId: string;
  }): Promise<WebhookRegistrationRow>;
  getRegistration(id: string): Promise<WebhookRegistrationRow | null>;
  listRegistrations(): Promise<WebhookRegistrationRow[]>;
  deleteRegistration(id: string): Promise<boolean>;

  getSample(id: string): Promise<WebhookSampleRow | null>;
  getLatestSample(registrationId: string, eventKey?: string): Promise<WebhookSampleRow | null>;
  listSamples(registrationId: string, eventKey: string | undefined, limit: number): Promise<WebhookSampleRow[]>;
  listObservedEventKeys(registrationId: string): Promise<string[]>;
}

export interface DueCronAutomation extends AutomationRow {
  trigger: Extract<AutomationTrigger, { kind: "cron" }>;
  nextFireAt: Date;
}

export interface CronRunClaim {
  /** A terminal row proves DBOS already ran this occurrence. It is returned so
   * a scheduler recovering after start-before-advance can finish the CAS. */
  kind: "claimed" | "terminal";
  run: AutomationRunRow;
}

export interface AutomationCronStore {
  listDueCron(now: Date, limit?: number): Promise<DueCronAutomation[]>;
  claimCronOccurrence(input: {
    automationId: string;
    scheduledFor: Date;
    leaseOwner: string;
    leaseExpiresAt: Date;
    now: Date;
  }): Promise<CronRunClaim | null>;
  markRunSkipped(runId: string, reason: string): Promise<void>;
  advanceCronSchedule(input: {
    automationId: string;
    scheduledFor: Date;
    nextFireAt: Date;
    fired: boolean;
    now: Date;
  }): Promise<boolean>;
}

export interface AutomationWorkflowStore {
  ensureRun(input: {
    id: string;
    automationId: string;
    trigger: AutomationRunTrigger;
    scheduledFor: Date | null;
  }): Promise<AutomationRunRow>;
  getRun(id: string): Promise<AutomationRunRow | null>;
  getAutomation(id: string): Promise<AutomationRow | null>;
  recordRendered(runId: string, prompt: string, title: string | null): Promise<void>;
  ensureAutomationTask(input: {
    runId: string;
    automationId: string;
    title: string | null;
    source: Record<string, unknown>;
  }): Promise<string>;
  getAutomationTaskSession(runId: string): Promise<string | null>;
  markRunRenderFailed(runId: string, error: string): Promise<void>;
  markRunLaunchFailed(runId: string, error: string): Promise<void>;
  markRunLaunched(runId: string, taskId: string, sessionId: string): Promise<void>;
}

function automationRow(row: typeof automationTable.$inferSelect): AutomationRow {
  return {
    ...row,
    createdByUserId: row.createdByUserId ?? null,
    nextFireAt: row.nextFireAt ?? null,
    lastFiredAt: row.lastFiredAt ?? null,
    archivedAt: row.archivedAt ?? null,
  };
}

function runRow(row: typeof automationRunTable.$inferSelect): AutomationRunRow {
  return {
    ...row,
    renderedPrompt: row.renderedPrompt ?? null,
    renderedTitle: row.renderedTitle ?? null,
    taskId: row.taskId ?? null,
    sessionId: row.sessionId ?? null,
    error: row.error ?? null,
    scheduledFor: row.scheduledFor ?? null,
    leaseOwner: row.leaseOwner ?? null,
    leaseExpiresAt: row.leaseExpiresAt ?? null,
  };
}

function registrationRow(
  row: typeof webhookRegistrationTable.$inferSelect,
): WebhookRegistrationRow {
  return {
    ...row,
    providerHint: row.providerHint ?? null,
    createdByUserId: row.createdByUserId ?? null,
  };
}

function sampleRow(row: typeof webhookSampleTable.$inferSelect): WebhookSampleRow {
  return row;
}

export function makeAutomationStore(
  db: ReturnType<typeof getDb> = getDb(),
): AutomationStore & AutomationCronStore & AutomationWorkflowStore {
  return {
    async list({ includeArchived }) {
      const rows = includeArchived
        ? await db.select().from(automationTable).orderBy(automationTable.name)
        : await db
            .select()
            .from(automationTable)
            .where(isNull(automationTable.archivedAt))
            .orderBy(automationTable.name);
      return rows.map(automationRow);
    },

    async get(id) {
      const [row] = await db.select().from(automationTable).where(eq(automationTable.id, id)).limit(1);
      return row ? automationRow(row) : null;
    },

    async getActive(id) {
      const [row] = await db
        .select()
        .from(automationTable)
        .where(and(eq(automationTable.id, id), isNull(automationTable.archivedAt)))
        .limit(1);
      return row ? automationRow(row) : null;
    },

    async listActiveForWebhookRegistration(registrationId) {
      const rows = await db
        .select()
        .from(automationTable)
        .where(
          and(
            isNull(automationTable.archivedAt),
            sql`${automationTable.trigger}->>'kind' = 'webhook'`,
            sql`${automationTable.trigger}->>'registrationId' = ${registrationId}`,
          ),
        )
        .orderBy(automationTable.id);
      return rows.map(automationRow);
    },

    async create(input, createdByUserId) {
      const id = crypto.randomUUID();
      await db.insert(automationTable).values({ id, ...input, createdByUserId });
      const row = await this.get(id);
      if (!row) throw new Error(`automation ${id} disappeared after insert`);
      return row;
    },

    async update(id, input) {
      const [row] = await db
        .update(automationTable)
        .set({ ...input, updatedAt: new Date() })
        .where(and(eq(automationTable.id, id), isNull(automationTable.archivedAt)))
        .returning();
      return row ? automationRow(row) : null;
    },

    async archive(id) {
      const now = new Date();
      const [row] = await db
        .update(automationTable)
        .set({ archivedAt: now, enabled: false, nextFireAt: null, updatedAt: now })
        .where(and(eq(automationTable.id, id), isNull(automationTable.archivedAt)))
        .returning();
      if (row) return automationRow(row);
      return this.get(id);
    },

    async setEnabled(id, enabled, nextFireAt) {
      const [row] = await db
        .update(automationTable)
        .set({
          enabled,
          ...(nextFireAt !== undefined ? { nextFireAt } : {}),
          updatedAt: new Date(),
        })
        .where(and(eq(automationTable.id, id), isNull(automationTable.archivedAt)))
        .returning();
      return row ? automationRow(row) : null;
    },

    async listRuns(automationId, limit) {
      const rows = await db
        .select()
        .from(automationRunTable)
        .where(eq(automationRunTable.automationId, automationId))
        .orderBy(desc(automationRunTable.createdAt))
        .limit(limit);
      return rows.map(runRow);
    },

    async listDueCron(now, limit = 100) {
      const rows = await db
        .select()
        .from(automationTable)
        .where(
          and(
            eq(automationTable.enabled, true),
            isNull(automationTable.archivedAt),
            sql`${automationTable.trigger}->>'kind' = 'cron'`,
            lte(automationTable.nextFireAt, now),
          ),
        )
        .orderBy(asc(automationTable.nextFireAt), asc(automationTable.id))
        .limit(limit);
      return rows.map((raw) => {
        const row = automationRow(raw);
        if (row.trigger.kind !== "cron" || row.nextFireAt === null) {
          throw new Error(`due automation ${row.id} did not contain a cron occurrence`);
        }
        return { ...row, trigger: row.trigger, nextFireAt: row.nextFireAt };
      });
    },

    async claimCronOccurrence(input) {
      const trigger: AutomationRunTrigger = { source: "cron" };
      const inserted = await db
        .insert(automationRunTable)
        .values({
          id: crypto.randomUUID(),
          automationId: input.automationId,
          trigger,
          scheduledFor: input.scheduledFor,
          leaseOwner: input.leaseOwner,
          leaseExpiresAt: input.leaseExpiresAt,
        })
        .onConflictDoNothing()
        .returning();
      if (inserted[0]) return { kind: "claimed", run: runRow(inserted[0]) };

      // The expiry value is the compare-and-swap: after one contender updates
      // it, PostgreSQL re-evaluates this predicate for the next waiter.
      const reacquired = await db
        .update(automationRunTable)
        .set({
          leaseOwner: input.leaseOwner,
          leaseExpiresAt: input.leaseExpiresAt,
        })
        .where(
          and(
            eq(automationRunTable.automationId, input.automationId),
            eq(automationRunTable.scheduledFor, input.scheduledFor),
            eq(automationRunTable.status, "pending"),
            lte(automationRunTable.leaseExpiresAt, input.now),
          ),
        )
        .returning();
      if (reacquired[0]) return { kind: "claimed", run: runRow(reacquired[0]) };

      const [existing] = await db
        .select()
        .from(automationRunTable)
        .where(
          and(
            eq(automationRunTable.automationId, input.automationId),
            eq(automationRunTable.scheduledFor, input.scheduledFor),
          ),
        )
        .limit(1);
      if (!existing || existing.status === "pending") return null;
      return { kind: "terminal", run: runRow(existing) };
    },

    async markRunSkipped(runId, reason) {
      await db
        .update(automationRunTable)
        .set({
          status: "skipped",
          error: reason,
          leaseOwner: null,
          leaseExpiresAt: null,
        })
        .where(and(eq(automationRunTable.id, runId), eq(automationRunTable.status, "pending")));
    },

    async advanceCronSchedule(input) {
      const rows = await db
        .update(automationTable)
        .set({
          nextFireAt: input.nextFireAt,
          ...(input.fired ? { lastFiredAt: input.scheduledFor } : {}),
          updatedAt: input.now,
        })
        .where(
          and(
            eq(automationTable.id, input.automationId),
            eq(automationTable.enabled, true),
            isNull(automationTable.archivedAt),
            eq(automationTable.nextFireAt, input.scheduledFor),
          ),
        )
        .returning({ id: automationTable.id });
      return rows.length > 0;
    },

    async ensureRun(input) {
      await db
        .insert(automationRunTable)
        .values({
          id: input.id,
          automationId: input.automationId,
          trigger: input.trigger,
          scheduledFor: input.scheduledFor,
        })
        .onConflictDoNothing()
        .returning();
      const row = await this.getRun(input.id);
      if (!row) throw new Error(`automation run ${input.id} disappeared after insert`);
      if (row.automationId !== input.automationId) {
        throw new Error(`automation run ${input.id} belongs to a different automation`);
      }
      return row;
    },

    async getRun(id) {
      const [row] = await db
        .select()
        .from(automationRunTable)
        .where(eq(automationRunTable.id, id))
        .limit(1);
      return row ? runRow(row) : null;
    },

    async getAutomation(id) {
      return this.get(id);
    },

    async recordRendered(runId, prompt, title) {
      await db
        .update(automationRunTable)
        .set({ renderedPrompt: prompt, renderedTitle: title })
        .where(and(eq(automationRunTable.id, runId), eq(automationRunTable.status, "pending")));
    },

    async ensureAutomationTask(input) {
      // A stable task id makes a retried DBOS launch step reuse the task row.
      const taskId = `automation:${input.runId}`;
      await db.transaction(async (tx) => {
        await tx
          .insert(taskTable)
          .values({
            id: taskId,
            type: "automation",
            title: input.title,
            status: "working",
            createdByUserId: null,
            source: input.source,
          })
          .onConflictDoNothing();
        await tx
          .update(automationRunTable)
          .set({ taskId })
          .where(
            and(
              eq(automationRunTable.id, input.runId),
              eq(automationRunTable.automationId, input.automationId),
              eq(automationRunTable.status, "pending"),
            ),
          );
      });
      return taskId;
    },

    async getAutomationTaskSession(runId) {
      const [row] = await db
        .select({ sessionId: taskSessionTable.sessionId })
        .from(automationRunTable)
        .innerJoin(taskSessionTable, eq(taskSessionTable.taskId, automationRunTable.taskId))
        .where(
          and(
            eq(automationRunTable.id, runId),
            eq(taskSessionTable.role, "primary"),
          ),
        )
        .limit(1);
      return row?.sessionId ?? null;
    },

    async markRunRenderFailed(runId, error) {
      await db
        .update(automationRunTable)
        .set({ status: "render_failed", error, leaseOwner: null, leaseExpiresAt: null })
        .where(and(eq(automationRunTable.id, runId), eq(automationRunTable.status, "pending")));
    },

    async markRunLaunchFailed(runId, error) {
      await db
        .update(automationRunTable)
        .set({ status: "launch_failed", error, leaseOwner: null, leaseExpiresAt: null })
        .where(and(eq(automationRunTable.id, runId), eq(automationRunTable.status, "pending")));
    },

    async markRunLaunched(runId, taskId, sessionId) {
      await db
        .update(automationRunTable)
        .set({
          status: "launched",
          taskId,
          sessionId,
          error: null,
          leaseOwner: null,
          leaseExpiresAt: null,
        })
        .where(and(eq(automationRunTable.id, runId), eq(automationRunTable.status, "pending")));
    },

    async createRegistration(input) {
      await db.insert(webhookRegistrationTable).values(input);
      const row = await this.getRegistration(input.id);
      if (!row) throw new Error(`webhook registration ${input.id} disappeared after insert`);
      return row;
    },

    async getRegistration(id) {
      const [row] = await db
        .select()
        .from(webhookRegistrationTable)
        .where(eq(webhookRegistrationTable.id, id))
        .limit(1);
      return row ? registrationRow(row) : null;
    },

    async listRegistrations() {
      const rows = await db
        .select()
        .from(webhookRegistrationTable)
        .orderBy(webhookRegistrationTable.name);
      return rows.map(registrationRow);
    },

    async deleteRegistration(id) {
      const rows = await db
        .delete(webhookRegistrationTable)
        .where(eq(webhookRegistrationTable.id, id))
        .returning({ id: webhookRegistrationTable.id });
      return rows.length > 0;
    },

    async getSample(id) {
      const [row] = await db.select().from(webhookSampleTable).where(eq(webhookSampleTable.id, id)).limit(1);
      return row ? sampleRow(row) : null;
    },

    async getLatestSample(registrationId, eventKey) {
      const condition = eventKey
        ? and(
            eq(webhookSampleTable.registrationId, registrationId),
            eq(webhookSampleTable.eventKey, eventKey),
          )
        : eq(webhookSampleTable.registrationId, registrationId);
      const [row] = await db
        .select()
        .from(webhookSampleTable)
        .where(condition)
        .orderBy(desc(webhookSampleTable.receivedAt))
        .limit(1);
      return row ? sampleRow(row) : null;
    },

    async listSamples(registrationId, eventKey, limit) {
      const condition = eventKey
        ? and(
            eq(webhookSampleTable.registrationId, registrationId),
            eq(webhookSampleTable.eventKey, eventKey),
          )
        : eq(webhookSampleTable.registrationId, registrationId);
      const rows = await db
        .select()
        .from(webhookSampleTable)
        .where(condition)
        .orderBy(desc(webhookSampleTable.receivedAt))
        .limit(limit);
      return rows.map(sampleRow);
    },

    async listObservedEventKeys(registrationId) {
      const rows = await db
        .selectDistinct({ eventKey: webhookSampleTable.eventKey })
        .from(webhookSampleTable)
        .where(eq(webhookSampleTable.registrationId, registrationId));
      return rows.map((row) => row.eventKey).sort();
    },
  };
}
