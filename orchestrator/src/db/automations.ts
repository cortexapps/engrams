import { and, desc, eq, isNull, sql } from "drizzle-orm";

import { getDb } from "./client.ts";
import {
  automation as automationTable,
  automationRun as automationRunTable,
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
): AutomationStore {
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
