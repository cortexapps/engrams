import { and, asc, desc, eq, isNull, lte, ne, sql } from "drizzle-orm";

import { getDb } from "./client.ts";
import {
  automation as automationTable,
  automationConcurrencyClaim as claimTable,
  automationRun as automationRunTable,
  automationSession as automationSessionTable,
  automationStepRun as stepRunTable,
  automationVersion as versionTable,
  task as taskTable,
  taskSession as taskSessionTable,
  webhookRegistration as webhookRegistrationTable,
  webhookSample as webhookSampleTable,
  type AutomationAction,
  type AutomationRunTrigger,
  type AutomationSettings,
  type AutomationTrigger,
  type BlockDef,
  type InputFieldSpec,
  type WebhookVerification,
} from "./schema.ts";
import { makeWebhookAliasResolver } from "../automations/aliases.ts";
import {
  definitionFromLegacyAction,
  legacyActionFromDefinition,
} from "../automations/legacy-compat.ts";
import { ENGINE_VERSION, type AutomationDefinition } from "../automations/engine/definition.ts";
import type { EngineRunStore, EngineStepRecord } from "../automations/engine/deps.ts";
import type { RunSnapshot } from "../automations/engine/context.ts";
import { RUN_TERMINAL_STATUSES } from "../automations/engine/interpreter.ts";
import { cronDeliveryKey } from "../automations/ids.ts";
import type { WebhookAliasMapping } from "../automations/template.ts";

// ---------------------------------------------------------------------------
// Row shapes
// ---------------------------------------------------------------------------

/** Automation metadata without any definition fields. */
export interface AutomationMetaRow {
  id: string;
  name: string;
  description: string;
  enabled: boolean;
  kind: string;
  builtinKey: string | null;
  currentVersion: number;
  inputs: Record<string, unknown>;
  endSessionsOnFinish: boolean;
  createdByUserId: string | null;
  nextFireAt: Date | null;
  lastFiredAt: Date | null;
  createdAt: Date;
  updatedAt: Date;
  archivedAt: Date | null;
}

/** The legacy proto view (phase-3 removal): metadata + the reconstructed
 * trigger/action of a single-create_session-block definition. */
export interface AutomationRow extends AutomationMetaRow {
  trigger: AutomationTrigger;
  action: AutomationAction;
}

export interface AutomationVersionRow {
  automationId: string;
  version: number;
  trigger: AutomationTrigger;
  blocks: BlockDef[];
  inputsSchema: InputFieldSpec[];
  settings: AutomationSettings;
  createdByUserId: string | null;
  createdAt: Date;
}

/** An enabled automation joined with its current definition — what dispatch
 * and the scheduler act on (covers non-legacy graphs too). */
export interface DispatchTarget {
  automation: AutomationMetaRow;
  definition: AutomationDefinition;
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
  version: number;
  trigger: AutomationRunTrigger;
  deliveryKey: string | null;
  concurrencyKey: string | null;
  renderedPrompt: string | null;
  renderedTitle: string | null;
  taskId: string | null;
  sessionId: string | null;
  status: string;
  error: string | null;
  scheduledFor: Date | null;
  leaseOwner: string | null;
  leaseExpiresAt: Date | null;
  startedAt: Date | null;
  endedAt: Date | null;
  createdAt: Date;
}

export interface WebhookRegistrationRow {
  id: string;
  name: string;
  verification: WebhookVerification;
  providerHint: string | null;
  /** Non-null once deliveries are refused with 410 Gone (ADR 0119 D5). */
  disabledReason: string | null;
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

// ---------------------------------------------------------------------------
// Interfaces
// ---------------------------------------------------------------------------

export interface AutomationStore {
  list(opts: { includeArchived: boolean }): Promise<AutomationRow[]>;
  get(id: string): Promise<AutomationRow | null>;
  getActive(id: string): Promise<AutomationRow | null>;
  /** Non-archived automations whose CURRENT version references the
   * registration, INCLUDING disabled ones — the deletion guard's view. */
  listBoundToWebhookRegistration(registrationId: string): Promise<AutomationMetaRow[]>;
  /** Enabled, non-archived automations whose current version references the
   * registration — the dispatch view (covers non-legacy graphs). */
  listEnabledForWebhookRegistration(registrationId: string): Promise<DispatchTarget[]>;
  listEnabledForIntegrationTrigger(
    provider: string,
    connectionId: string,
  ): Promise<DispatchTarget[]>;
  create(input: AutomationInput, createdByUserId: string): Promise<AutomationRow>;
  update(id: string, input: AutomationInput): Promise<AutomationRow | null>;
  archive(id: string): Promise<AutomationRow | null>;
  setEnabled(
    id: string,
    enabled: boolean,
    nextFireAt?: Date | null,
  ): Promise<AutomationRow | null>;
  listRuns(automationId: string, limit: number): Promise<AutomationRunRow[]>;
  getVersion(automationId: string, version: number): Promise<AutomationVersionRow | null>;

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
  setRegistrationDisabledReason(id: string, reason: string | null): Promise<boolean>;
  /** Insert version current+1 with an arbitrary definition and repoint the
   * automation at it, in one transaction. The scheme-retirement backfill's
   * write path; the legacy proto path keeps using create/update. */
  replaceCurrentDefinition(
    automationId: string,
    definition: AutomationDefinition,
  ): Promise<AutomationMetaRow | null>;

  getSample(id: string): Promise<WebhookSampleRow | null>;
  recordWebhookSample(input: {
    registrationId: string;
    eventKey: string;
    payload: Record<string, unknown>;
    receivedAt: Date;
    retain: number;
  }): Promise<void>;
  getLatestSample(registrationId: string, eventKey?: string): Promise<WebhookSampleRow | null>;
  listSamples(registrationId: string, eventKey: string | undefined, limit: number): Promise<WebhookSampleRow[]>;
  listObservedEventKeys(registrationId: string): Promise<string[]>;
}

export interface DueCronAutomation {
  automation: AutomationMetaRow;
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
    runId: string;
    automationId: string;
    version: number;
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

export type ConcurrencyClaimResult = { claimed: true } | { claimed: false; holderRunId: string };

/** Dispatch-side run admission (ADR 0119 D4). */
export interface AutomationDispatchStore {
  insertRun(input: {
    id: string;
    automationId: string;
    version: number;
    trigger: AutomationRunTrigger;
    deliveryKey: string | null;
    concurrencyKey: string | null;
    scheduledFor: Date | null;
    status?: string;
    error?: string;
    endedAt?: Date;
  }): Promise<AutomationRunRow>;
  claimConcurrency(
    automationId: string,
    concurrencyKey: string,
    runId: string,
  ): Promise<ConcurrencyClaimResult>;
  casConcurrency(
    automationId: string,
    concurrencyKey: string,
    fromRunId: string,
    toRunId: string,
  ): Promise<boolean>;
}

/** Everything the interpreter workflow needs (EngineRunStore) plus the
 * production session-op helpers. */
export interface AutomationEngineStore extends EngineRunStore {
  getRun(id: string): Promise<AutomationRunRow | null>;
  ensureAutomationTask(input: {
    runId: string;
    automationId: string;
    title: string | null;
    source: Record<string, unknown>;
  }): Promise<string>;
  getAutomationTaskSession(runId: string): Promise<string | null>;
  recordSessionBinding(input: {
    sessionId: string;
    runId: string;
    blockId: string;
    role: string;
    keep: boolean;
  }): Promise<void>;
  /** Denormalize the primary launch onto the run row for the legacy list. */
  recordRunLaunch(input: {
    runId: string;
    taskId: string;
    sessionId: string;
    prompt: string;
    title: string | null;
  }): Promise<void>;
  findSessionBinding(sessionId: string): Promise<{ runId: string; blockId: string; role: string } | null>;
}

// ---------------------------------------------------------------------------
// Row mappers
// ---------------------------------------------------------------------------

function metaRow(row: typeof automationTable.$inferSelect): AutomationMetaRow {
  return {
    id: row.id,
    name: row.name,
    description: row.description,
    enabled: row.enabled,
    kind: row.kind,
    builtinKey: row.builtinKey ?? null,
    currentVersion: row.currentVersion,
    inputs: row.inputs,
    endSessionsOnFinish: row.endSessionsOnFinish,
    createdByUserId: row.createdByUserId ?? null,
    nextFireAt: row.nextFireAt ?? null,
    lastFiredAt: row.lastFiredAt ?? null,
    createdAt: row.createdAt,
    updatedAt: row.updatedAt,
    archivedAt: row.archivedAt ?? null,
  };
}

function versionRow(row: typeof versionTable.$inferSelect): AutomationVersionRow {
  return {
    automationId: row.automationId,
    version: row.version,
    trigger: row.trigger,
    blocks: row.blocks,
    inputsSchema: row.inputsSchema,
    settings: row.settings,
    createdByUserId: row.createdByUserId ?? null,
    createdAt: row.createdAt,
  };
}

export function definitionOf(version: AutomationVersionRow): AutomationDefinition {
  return {
    engine: ENGINE_VERSION,
    trigger: version.trigger,
    blocks: version.blocks,
    inputsSchema: version.inputsSchema,
    settings: version.settings,
  };
}

function runRow(row: typeof automationRunTable.$inferSelect): AutomationRunRow {
  return {
    id: row.id,
    automationId: row.automationId,
    version: row.version,
    trigger: row.trigger,
    deliveryKey: row.deliveryKey ?? null,
    concurrencyKey: row.concurrencyKey ?? null,
    renderedPrompt: row.renderedPrompt ?? null,
    renderedTitle: row.renderedTitle ?? null,
    taskId: row.taskId ?? null,
    sessionId: row.sessionId ?? null,
    status: row.status,
    error: row.error ?? null,
    scheduledFor: row.scheduledFor ?? null,
    leaseOwner: row.leaseOwner ?? null,
    leaseExpiresAt: row.leaseExpiresAt ?? null,
    startedAt: row.startedAt ?? null,
    endedAt: row.endedAt ?? null,
    createdAt: row.createdAt,
  };
}

function registrationRow(
  row: typeof webhookRegistrationTable.$inferSelect,
): WebhookRegistrationRow {
  return {
    ...row,
    providerHint: row.providerHint ?? null,
    disabledReason: row.disabledReason ?? null,
    createdByUserId: row.createdByUserId ?? null,
  };
}

function sampleRow(row: typeof webhookSampleTable.$inferSelect): WebhookSampleRow {
  return row;
}

/** Legacy view: meta + reconstructed trigger/action. Null when the current
 * definition is not a single create_session block (builtins, multi-block). */
function legacyRow(
  meta: AutomationMetaRow,
  version: AutomationVersionRow,
): AutomationRow | null {
  if (meta.kind !== "user") return null;
  const action = legacyActionFromDefinition(definitionOf(version));
  if (action === null) return null;
  return { ...meta, trigger: version.trigger, action };
}

/** Resolve input values: schema defaults overlaid by stored values. */
export function resolveAutomationInputs(
  schema: InputFieldSpec[],
  values: Record<string, unknown>,
): Record<string, unknown> {
  const resolved: Record<string, unknown> = Object.create(null);
  for (const field of schema) {
    if (field.default !== undefined) resolved[field.key] = field.default;
  }
  for (const [key, value] of Object.entries(values)) {
    resolved[key] = value;
  }
  return resolved;
}

// Derived from the interpreter's source-of-truth array so a new terminal
// status is a compile-visible change here, never a silently frozen cron
// schedule (claimCronOccurrence treats non-terminal as "occurrence taken").
const TERMINAL_RUN_STATUSES: ReadonlySet<string> = new Set(RUN_TERMINAL_STATUSES);

// ---------------------------------------------------------------------------
// Store factory
// ---------------------------------------------------------------------------

export function makeAutomationStore(
  db: ReturnType<typeof getDb> = getDb(),
): AutomationStore & AutomationCronStore & AutomationDispatchStore {
  async function currentVersionOf(meta: AutomationMetaRow): Promise<AutomationVersionRow> {
    const [row] = await db
      .select()
      .from(versionTable)
      .where(
        and(eq(versionTable.automationId, meta.id), eq(versionTable.version, meta.currentVersion)),
      )
      .limit(1);
    if (!row) throw new Error(`automation ${meta.id} is missing version ${meta.currentVersion}`);
    return versionRow(row);
  }

  async function legacyView(meta: AutomationMetaRow): Promise<AutomationRow | null> {
    return legacyRow(meta, await currentVersionOf(meta));
  }

  async function listWithCurrentVersions(where: ReturnType<typeof and>): Promise<
    Array<{ meta: AutomationMetaRow; version: AutomationVersionRow }>
  > {
    const rows = await db
      .select({ automation: automationTable, version: versionTable })
      .from(automationTable)
      .innerJoin(
        versionTable,
        and(
          eq(versionTable.automationId, automationTable.id),
          eq(versionTable.version, automationTable.currentVersion),
        ),
      )
      .where(where)
      .orderBy(automationTable.name);
    return rows.map((row) => ({ meta: metaRow(row.automation), version: versionRow(row.version) }));
  }

  return {
    async list({ includeArchived }) {
      const rows = await listWithCurrentVersions(
        includeArchived ? undefined : isNull(automationTable.archivedAt),
      );
      return rows
        .map(({ meta, version }) => legacyRow(meta, version))
        .filter((row): row is AutomationRow => row !== null);
    },

    async get(id) {
      const [row] = await db.select().from(automationTable).where(eq(automationTable.id, id)).limit(1);
      return row ? legacyView(metaRow(row)) : null;
    },

    async getActive(id) {
      const [row] = await db
        .select()
        .from(automationTable)
        .where(and(eq(automationTable.id, id), isNull(automationTable.archivedAt)))
        .limit(1);
      return row ? legacyView(metaRow(row)) : null;
    },

    async listBoundToWebhookRegistration(registrationId) {
      const rows = await listWithCurrentVersions(
        and(
          isNull(automationTable.archivedAt),
          sql`${versionTable.trigger}->>'kind' = 'webhook'`,
          sql`${versionTable.trigger}->>'registrationId' = ${registrationId}`,
        ),
      );
      return rows.map(({ meta }) => meta);
    },

    async listEnabledForWebhookRegistration(registrationId) {
      const rows = await listWithCurrentVersions(
        and(
          eq(automationTable.enabled, true),
          isNull(automationTable.archivedAt),
          sql`${versionTable.trigger}->>'kind' = 'webhook'`,
          sql`${versionTable.trigger}->>'registrationId' = ${registrationId}`,
        ),
      );
      return rows.map(({ meta, version }) => ({
        automation: meta,
        definition: definitionOf(version),
      }));
    },

    async listEnabledForIntegrationTrigger(provider, connectionId) {
      const rows = await listWithCurrentVersions(
        and(
          eq(automationTable.enabled, true),
          isNull(automationTable.archivedAt),
          sql`${versionTable.trigger}->>'kind' = 'integration'`,
          sql`${versionTable.trigger}->>'provider' = ${provider}`,
          sql`${versionTable.trigger}->>'connectionId' = ${connectionId}`,
        ),
      );
      return rows.map(({ meta, version }) => ({
        automation: meta,
        definition: definitionOf(version),
      }));
    },

    async create(input, createdByUserId) {
      const id = crypto.randomUUID();
      const definition = definitionFromLegacyAction(input.trigger, input.action);
      await db.transaction(async (tx) => {
        await tx.insert(automationTable).values({
          id,
          name: input.name,
          description: input.description,
          enabled: input.enabled,
          currentVersion: 1,
          nextFireAt: input.nextFireAt,
          createdByUserId,
        });
        await tx.insert(versionTable).values({
          automationId: id,
          version: 1,
          trigger: definition.trigger,
          blocks: definition.blocks,
          inputsSchema: definition.inputsSchema,
          settings: definition.settings,
          createdByUserId,
        });
      });
      const row = await this.get(id);
      if (!row) throw new Error(`automation ${id} disappeared after insert`);
      return row;
    },

    async update(id, input) {
      const definition = definitionFromLegacyAction(input.trigger, input.action);
      const updated = await db.transaction(async (tx) => {
        const [existing] = await tx
          .select()
          .from(automationTable)
          .where(and(eq(automationTable.id, id), isNull(automationTable.archivedAt)))
          .for("update")
          .limit(1);
        if (!existing || existing.kind !== "user") return false;
        const nextVersion = existing.currentVersion + 1;
        await tx.insert(versionTable).values({
          automationId: id,
          version: nextVersion,
          trigger: definition.trigger,
          blocks: definition.blocks,
          inputsSchema: definition.inputsSchema,
          settings: definition.settings,
          createdByUserId: existing.createdByUserId ?? null,
        });
        await tx
          .update(automationTable)
          .set({
            name: input.name,
            description: input.description,
            enabled: input.enabled,
            nextFireAt: input.nextFireAt,
            currentVersion: nextVersion,
            updatedAt: new Date(),
          })
          .where(eq(automationTable.id, id));
        return true;
      });
      if (!updated) return null;
      return this.get(id);
    },

    async archive(id) {
      const now = new Date();
      await db
        .update(automationTable)
        .set({ archivedAt: now, enabled: false, nextFireAt: null, updatedAt: now })
        .where(and(eq(automationTable.id, id), isNull(automationTable.archivedAt)));
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
      return row ? legacyView(metaRow(row)) : null;
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

    async getVersion(automationId, version) {
      const [row] = await db
        .select()
        .from(versionTable)
        .where(and(eq(versionTable.automationId, automationId), eq(versionTable.version, version)))
        .limit(1);
      return row ? versionRow(row) : null;
    },

    // -----------------------------------------------------------------------
    // Cron
    // -----------------------------------------------------------------------

    async listDueCron(now, limit = 100) {
      const rows = await db
        .select({ automation: automationTable, version: versionTable })
        .from(automationTable)
        .innerJoin(
          versionTable,
          and(
            eq(versionTable.automationId, automationTable.id),
            eq(versionTable.version, automationTable.currentVersion),
          ),
        )
        .where(
          and(
            eq(automationTable.enabled, true),
            isNull(automationTable.archivedAt),
            sql`${versionTable.trigger}->>'kind' = 'cron'`,
            lte(automationTable.nextFireAt, now),
          ),
        )
        .orderBy(asc(automationTable.nextFireAt), asc(automationTable.id))
        .limit(limit);
      return rows.map((raw) => {
        const meta = metaRow(raw.automation);
        const version = versionRow(raw.version);
        if (version.trigger.kind !== "cron" || meta.nextFireAt === null) {
          throw new Error(`due automation ${meta.id} did not contain a cron occurrence`);
        }
        return { automation: meta, trigger: version.trigger, nextFireAt: meta.nextFireAt };
      });
    },

    async claimCronOccurrence(input) {
      const trigger: AutomationRunTrigger = {
        source: "cron",
        receivedAt: input.scheduledFor.toISOString(),
      };
      const inserted = await db
        .insert(automationRunTable)
        .values({
          id: input.runId,
          automationId: input.automationId,
          version: input.version,
          trigger,
          deliveryKey: cronDeliveryKey(input.scheduledFor),
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
      if (!existing) return null;
      if (!TERMINAL_RUN_STATUSES.has(existing.status)) return null;
      return { kind: "terminal", run: runRow(existing) };
    },

    async markRunSkipped(runId, reason) {
      await db
        .update(automationRunTable)
        .set({
          status: "filtered",
          error: reason,
          endedAt: new Date(),
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

    // -----------------------------------------------------------------------
    // Dispatch admission
    // -----------------------------------------------------------------------

    async insertRun(input) {
      await db
        .insert(automationRunTable)
        .values({
          id: input.id,
          automationId: input.automationId,
          version: input.version,
          trigger: input.trigger,
          deliveryKey: input.deliveryKey,
          concurrencyKey: input.concurrencyKey,
          scheduledFor: input.scheduledFor,
          ...(input.status !== undefined ? { status: input.status } : {}),
          ...(input.error !== undefined ? { error: input.error } : {}),
          ...(input.endedAt !== undefined ? { endedAt: input.endedAt } : {}),
        })
        .onConflictDoNothing();
      const [row] = await db
        .select()
        .from(automationRunTable)
        .where(eq(automationRunTable.id, input.id))
        .limit(1);
      if (!row) throw new Error(`automation run ${input.id} disappeared after insert`);
      if (row.automationId !== input.automationId) {
        throw new Error(`automation run ${input.id} belongs to a different automation`);
      }
      return runRow(row);
    },

    async claimConcurrency(automationId, concurrencyKey, runId) {
      const inserted = await db
        .insert(claimTable)
        .values({ automationId, concurrencyKey, runId })
        .onConflictDoNothing()
        .returning();
      if (inserted[0]) return { claimed: true };
      const [holder] = await db
        .select()
        .from(claimTable)
        .where(
          and(
            eq(claimTable.automationId, automationId),
            eq(claimTable.concurrencyKey, concurrencyKey),
          ),
        )
        .limit(1);
      // The holder released between our insert and read: claim again.
      if (!holder) return this.claimConcurrency(automationId, concurrencyKey, runId);
      if (holder.runId === runId) return { claimed: true };
      return { claimed: false, holderRunId: holder.runId };
    },

    async casConcurrency(automationId, concurrencyKey, fromRunId, toRunId) {
      const rows = await db
        .update(claimTable)
        .set({ runId: toRunId, claimedAt: new Date() })
        .where(
          and(
            eq(claimTable.automationId, automationId),
            eq(claimTable.concurrencyKey, concurrencyKey),
            eq(claimTable.runId, fromRunId),
          ),
        )
        .returning({ runId: claimTable.runId });
      return rows.length > 0;
    },

    // -----------------------------------------------------------------------
    // Webhook registrations + samples (unchanged from ADR 0102)
    // -----------------------------------------------------------------------

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

    async setRegistrationDisabledReason(id, reason) {
      const rows = await db
        .update(webhookRegistrationTable)
        .set({ disabledReason: reason })
        .where(eq(webhookRegistrationTable.id, id))
        .returning({ id: webhookRegistrationTable.id });
      return rows.length > 0;
    },

    async replaceCurrentDefinition(automationId, definition) {
      return db.transaction(async (tx) => {
        const [current] = await tx
          .select({ currentVersion: automationTable.currentVersion })
          .from(automationTable)
          .where(eq(automationTable.id, automationId))
          .limit(1);
        if (!current) return null;
        const version = current.currentVersion + 1;
        await tx.insert(versionTable).values({
          automationId,
          version,
          trigger: definition.trigger,
          blocks: definition.blocks,
          inputsSchema: definition.inputsSchema,
          settings: definition.settings,
          createdByUserId: null,
        });
        await tx
          .update(automationTable)
          .set({ currentVersion: version, updatedAt: new Date() })
          .where(eq(automationTable.id, automationId));
        return (await this.get(automationId)) ?? null;
      });
    },

    async getSample(id) {
      const [row] = await db.select().from(webhookSampleTable).where(eq(webhookSampleTable.id, id)).limit(1);
      return row ? sampleRow(row) : null;
    },

    async recordWebhookSample(input) {
      if (!Number.isSafeInteger(input.retain) || input.retain < 1) {
        throw new RangeError("webhook sample retention must be a positive safe integer");
      }
      await db.transaction(async (tx) => {
        // Serialize writers per registration before insert+prune. Without this
        // lock, two concurrent deliveries can both retain N rows from their
        // snapshots and commit N+1 rows despite pruning in each transaction.
        await tx.execute(sql`
          select id from ${webhookRegistrationTable}
          where id = ${input.registrationId}
          for update
        `);
        await tx.insert(webhookSampleTable).values({
          registrationId: input.registrationId,
          eventKey: input.eventKey,
          payload: input.payload,
          receivedAt: input.receivedAt,
        });
        await tx.execute(sql`
          delete from ${webhookSampleTable}
          where registration_id = ${input.registrationId}
            and event_key = ${input.eventKey}
            and id not in (
              select id from ${webhookSampleTable}
              where registration_id = ${input.registrationId}
                and event_key = ${input.eventKey}
              order by received_at desc, id desc
              limit ${input.retain}
            )
        `);
      });
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

// ---------------------------------------------------------------------------
// Engine store (EngineRunStore + session-op helpers)
// ---------------------------------------------------------------------------

export interface EngineStoreDeps {
  db?: ReturnType<typeof getDb>;
  aliases?: (registrationId: string) => Promise<readonly WebhookAliasMapping[]>;
  now?: () => Date;
}

export function makeAutomationEngineStore(deps: EngineStoreDeps = {}): AutomationEngineStore {
  const db = deps.db ?? getDb();
  const now = deps.now ?? (() => new Date());
  let aliasResolver = deps.aliases;

  async function getRun(id: string): Promise<AutomationRunRow | null> {
    const [row] = await db
      .select()
      .from(automationRunTable)
      .where(eq(automationRunTable.id, id))
      .limit(1);
    return row ? runRow(row) : null;
  }

  return {
    getRun,

    async loadSnapshot(runId): Promise<RunSnapshot> {
      const run = await getRun(runId);
      if (!run) throw new Error(`automation run ${runId} not found`);
      const [automationRaw] = await db
        .select()
        .from(automationTable)
        .where(eq(automationTable.id, run.automationId))
        .limit(1);
      if (!automationRaw) throw new Error(`automation ${run.automationId} not found`);
      const meta = metaRow(automationRaw);
      const [versionRaw] = await db
        .select()
        .from(versionTable)
        .where(
          and(eq(versionTable.automationId, run.automationId), eq(versionTable.version, run.version)),
        )
        .limit(1);
      if (!versionRaw) {
        throw new Error(`automation ${run.automationId} is missing version ${run.version}`);
      }
      const version = versionRow(versionRaw);
      const definition = definitionOf(version);

      let aliases: readonly WebhookAliasMapping[] = [];
      if (definition.trigger.kind === "webhook") {
        aliasResolver ??= makeWebhookAliasResolver();
        aliases = await aliasResolver(definition.trigger.registrationId);
      }

      return {
        definition,
        inputs: resolveAutomationInputs(version.inputsSchema, meta.inputs),
        automationId: meta.id,
        automationName: meta.name,
        version: version.version,
        trigger: {
          kind: run.trigger.source,
          receivedAt: run.trigger.receivedAt ?? run.createdAt.toISOString(),
          ...(run.trigger.eventKey !== undefined ? { eventKey: run.trigger.eventKey } : {}),
          ...(run.scheduledFor ? { scheduledFor: run.scheduledFor.toISOString() } : {}),
          ...(run.deliveryKey !== null ? { deliveryKey: run.deliveryKey } : {}),
          ...(run.trigger.payload !== undefined ? { payload: run.trigger.payload } : {}),
        },
        aliases: [...aliases],
        ...(run.concurrencyKey !== null ? { concurrencyKey: run.concurrencyKey } : {}),
        startedAtMs: run.startedAt?.getTime() ?? now().getTime(),
      };
    },

    async markRunning(runId, startedAtMs) {
      await db
        .update(automationRunTable)
        .set({
          status: "running",
          startedAt: new Date(startedAtMs),
          leaseOwner: null,
          leaseExpiresAt: null,
        })
        .where(
          and(
            eq(automationRunTable.id, runId),
            sql`${automationRunTable.status} in ('pending', 'running')`,
          ),
        );
    },

    async recordStep(runId, framePath, attempt, record: EngineStepRecord) {
      const endedAt = record.status === "running" ? null : now();
      await db
        .insert(stepRunTable)
        .values({
          runId,
          blockId: framePath,
          attempt,
          status: record.status,
          inputs: record.inputs ?? null,
          outputs: record.outputs ?? null,
          error: record.error ?? null,
          startedAt: now(),
          endedAt,
        })
        .onConflictDoUpdate({
          target: [stepRunTable.runId, stepRunTable.blockId, stepRunTable.attempt],
          set: {
            status: record.status,
            ...(record.inputs !== undefined ? { inputs: record.inputs } : {}),
            ...(record.outputs !== undefined ? { outputs: record.outputs } : {}),
            ...(record.error !== undefined ? { error: record.error } : {}),
            endedAt,
          },
        });
    },

    async finalizeRun(runId, status, error) {
      await db
        .update(automationRunTable)
        .set({
          status,
          error: error ?? null,
          endedAt: now(),
          leaseOwner: null,
          leaseExpiresAt: null,
        })
        .where(eq(automationRunTable.id, runId));
    },

    async listRunSessions(runId) {
      const rows = await db
        .select({ sessionId: automationSessionTable.sessionId, keep: automationSessionTable.keep })
        .from(automationSessionTable)
        .where(eq(automationSessionTable.runId, runId));
      return rows;
    },

    async releaseConcurrency(runId) {
      const run = await getRun(runId);
      if (!run || run.concurrencyKey === null) return null;
      const key = run.concurrencyKey;
      return db.transaction(async (tx) => {
        const released = await tx
          .delete(claimTable)
          .where(
            and(
              eq(claimTable.automationId, run.automationId),
              eq(claimTable.concurrencyKey, key),
              eq(claimTable.runId, runId),
            ),
          )
          .returning({ runId: claimTable.runId });
        // Someone CAS'd the claim away (supersede): nothing to promote.
        if (released.length === 0) return null;
        const [successor] = await tx
          .select({ id: automationRunTable.id })
          .from(automationRunTable)
          .where(
            and(
              eq(automationRunTable.automationId, run.automationId),
              eq(automationRunTable.concurrencyKey, key),
              eq(automationRunTable.status, "pending"),
              // The releasing run may itself still read as pending (e.g. it
              // never reached markRunning); it must never promote itself.
              ne(automationRunTable.id, runId),
            ),
          )
          .orderBy(asc(automationRunTable.createdAt), asc(automationRunTable.id))
          .limit(1);
        if (!successor) return null;
        const claimed = await tx
          .insert(claimTable)
          .values({ automationId: run.automationId, concurrencyKey: key, runId: successor.id })
          .onConflictDoNothing()
          .returning({ runId: claimTable.runId });
        return claimed.length > 0 ? successor.id : null;
      });
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

    async recordSessionBinding(input) {
      await db
        .insert(automationSessionTable)
        .values({
          sessionId: input.sessionId,
          runId: input.runId,
          blockId: input.blockId,
          role: input.role,
          keep: input.keep,
        })
        .onConflictDoNothing();
    },

    async recordRunLaunch(input) {
      await db
        .update(automationRunTable)
        .set({
          taskId: input.taskId,
          sessionId: input.sessionId,
          renderedPrompt: input.prompt,
          renderedTitle: input.title,
        })
        .where(eq(automationRunTable.id, input.runId));
    },

    async findSessionBinding(sessionId) {
      const [row] = await db
        .select({
          runId: automationSessionTable.runId,
          blockId: automationSessionTable.blockId,
          role: automationSessionTable.role,
        })
        .from(automationSessionTable)
        .where(eq(automationSessionTable.sessionId, sessionId))
        .limit(1);
      return row ?? null;
    },
  };
}
