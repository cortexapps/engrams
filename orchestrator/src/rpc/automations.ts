import { create } from "@bufbuild/protobuf";
import { ConnectError, Code } from "@connectrpc/connect";
import type { ConnectRouter } from "@connectrpc/connect";
import { Cron } from "croner";

import {
  AutomationRunSchema,
  AutomationActionSchema,
  AutomationSchema,
  AutomationService,
  AutomationTriggerSchema,
  WebhookRegistrationSchema,
  WebhookRegistrationService,
  WebhookSampleSchema,
  type Automation as ProtoAutomation,
  type AutomationAction as ProtoAutomationAction,
  type AutomationRun as ProtoAutomationRun,
  type AutomationTrigger as ProtoAutomationTrigger,
  type WebhookRegistration as ProtoWebhookRegistration,
  type WebhookSample as ProtoWebhookSample,
} from "../gen/engram/app/v1/automation_pb.ts";
import { getSessionFromHeaders } from "../auth/session.ts";
import { requireAdmin } from "./require.ts";
import { evaluateCode, type CodeInput } from "../automations/code/sandbox.ts";
import { CODE_SOURCE_MAX_CHARS } from "../automations/engine/blocks/code.ts";
import {
  makeAutomationStore,
  type AutomationInput,
  type AutomationRow,
  type AutomationRunRow,
  type AutomationStore,
  type WebhookRegistrationRow,
  type WebhookSampleRow,
} from "../db/automations.ts";
import { getDb } from "../db/client.ts";
import { makeConnectorStore } from "../db/connectors.ts";
import { makeProfileStore, type ProfileStore } from "../db/profiles.ts";
import type {
  AutomationAction,
  AutomationTrigger,
  CreateTaskAutomationAction,
  WebhookVerificationScheme,
} from "../db/schema.ts";
import {
  type Connector,
  type CustomConnectorSource,
  loadRegistry,
  type WebhookAliasSpec,
} from "../connectors/registry.ts";
import { loadEventSample } from "../connectors/samples.ts";
import {
  makeIntegrationEventStore,
  type IntegrationEventStore,
} from "../db/integration-events.ts";
import {
  makeIntegrationConnectionStore,
  type IntegrationConnectionStore,
} from "../db/integration-connections.ts";
import {
  harnessCatalog as defaultHarnessCatalog,
  orgSecret as defaultOrgSecret,
} from "../control-plane/client.ts";
import type { HarnessCatalogClient } from "./task-create.ts";
import {
  AutomationTemplateError,
  buildAutomationTemplateContext,
  renderAutomationAction,
  renderAutomationTemplate,
  validateAutomationTemplate,
} from "../automations/template.ts";
import { SYSTEM_GITHUB_REGISTRATION_ID } from "../automations/webhook.ts";
import { legacyRunStatus } from "../automations/legacy-compat.ts";
import { makeModelRouterStore, type ModelRouterStore } from "../db/model-routers.ts";
import { getModelRouterDefinition, selectRouterProtocol } from "../model-routers/registry.ts";

export type GetSession = (
  headers: Headers,
) => Promise<{ user: { id: string; role?: string | null } } | null>;

export interface OrgSecretClient {
  putSecret(req: { name: string; value: string }): Promise<unknown>;
  deleteSecret(req: { name: string }): Promise<{ deleted: boolean }>;
}

export interface AutomationDeps {
  getSession?: GetSession;
  store?: AutomationStore;
  profiles?: Pick<ProfileStore, "getActive">;
  connectors?: CustomConnectorSource;
  harnessCatalog?: HarnessCatalogClient;
  orgSecret?: OrgSecretClient;
  modelRouters?: ModelRouterStore;
  integrationEvents?: Pick<IntegrationEventStore, "getLatest" | "listObservedEventKeys">;
  connections?: Pick<IntegrationConnectionStore, "getDefault">;
  eventSample?: typeof loadEventSample;
  now?: () => Date;
  randomSecret?: () => string;
  /** Test seam for the QuickJS sandbox behind EvalCode. */
  evalCode?: typeof evaluateCode;
}

export const EVAL_CODE_LIMIT_PER_MINUTE = 30;
export const EVAL_CODE_INPUT_MAX_CHARS = 512 * 1024;

/** Sliding-window per-user limiter for the arbitrary-compute endpoint.
 * In-memory per pod: the cap is a courtesy brake, not an SLO. */
function makeEvalRateLimiter() {
  const windows = new Map<string, number[]>();
  return {
    allow(userId: string, nowMs: number): boolean {
      const cutoff = nowMs - 60_000;
      const stamps = (windows.get(userId) ?? []).filter((t) => t > cutoff);
      if (stamps.length >= EVAL_CODE_LIMIT_PER_MINUTE) {
        windows.set(userId, stamps);
        return false;
      }
      stamps.push(nowMs);
      windows.set(userId, stamps);
      return true;
    },
  };
}

const REGISTRATION_ID_RE = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/;
const EVENT_KEY_RE = /^[a-z0-9_-]+(?:\.[a-z0-9_-]+)*$/;
const DEFAULT_LIST_LIMIT = 50;
const MAX_LIST_LIMIT = 200;
const MAX_WEBHOOK_EVENTS = 200;
const MAX_EVENT_KEY_LENGTH = 160;
const MAX_WEBHOOK_FILTER_PATHS = 100;
const MAX_WEBHOOK_FILTER_PATH_LENGTH = 512;
const WEBHOOK_FILTER_PATH_RE = /^[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)*$/;
const UNSAFE_FILTER_PATH_SEGMENTS = new Set(["__proto__", "constructor", "prototype"]);

function assertWebhookFilter(filter: Record<string, unknown>): void {
  const paths = Object.keys(filter);
  if (paths.length > MAX_WEBHOOK_FILTER_PATHS) {
    throw new ConnectError(
      `webhook filter may contain at most ${MAX_WEBHOOK_FILTER_PATHS} paths`,
      Code.InvalidArgument,
    );
  }
  if (paths.some((path) =>
    path.length > MAX_WEBHOOK_FILTER_PATH_LENGTH
    || !WEBHOOK_FILTER_PATH_RE.test(path)
    || path.split(".").some((segment) => UNSAFE_FILTER_PATH_SEGMENTS.has(segment)))) {
    throw new ConnectError(
      "webhook filter keys must be safe dotted payload paths",
      Code.InvalidArgument,
    );
  }
}


function requiredText(value: string, field: string): string {
  const trimmed = value.trim();
  if (!trimmed) throw new ConnectError(`${field} is required`, Code.InvalidArgument);
  return trimmed;
}

function listLimit(value: number): number {
  if (!Number.isInteger(value) || value < 0) {
    throw new ConnectError("limit must be a non-negative integer", Code.InvalidArgument);
  }
  return value === 0 ? DEFAULT_LIST_LIMIT : Math.min(value, MAX_LIST_LIMIT);
}

function parseObjectJson(value: string, field: string): Record<string, unknown> {
  let parsed: unknown;
  try {
    parsed = JSON.parse(value);
  } catch (error) {
    throw new ConnectError(
      `${field} is not valid JSON: ${error instanceof Error ? error.message : String(error)}`,
      Code.InvalidArgument,
    );
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    throw new ConnectError(`${field} must be a JSON object`, Code.InvalidArgument);
  }
  return parsed as Record<string, unknown>;
}

function verificationScheme(value: string): WebhookVerificationScheme {
  switch (value) {
    case "github_hmac_sha256":
    case "slack_v0":
    case "generic_hmac_sha256":
      return value;
    default:
      throw new ConnectError("unknown verification_scheme", Code.InvalidArgument);
  }
}

function assertTimezone(timezone: string): void {
  if (!timezone) throw new ConnectError("cron timezone is required", Code.InvalidArgument);
  try {
    new Intl.DateTimeFormat("en-US", { timeZone: timezone }).format(new Date(0));
  } catch {
    throw new ConnectError(`invalid IANA timezone "${timezone}"`, Code.InvalidArgument);
  }
}

function nextCronFire(schedule: string, timezone: string, now: Date): Date {
  assertTimezone(timezone);
  try {
    const cron = new Cron(schedule, { timezone, paused: true });
    const next = cron.nextRun(now);
    if (!next) throw new Error("schedule has no future occurrence");
    return next;
  } catch (error) {
    throw new ConnectError(
      `invalid cron schedule: ${error instanceof Error ? error.message : String(error)}`,
      Code.InvalidArgument,
    );
  }
}

function parseTrigger(value: ProtoAutomationTrigger | undefined): AutomationTrigger {
  switch (value?.trigger.case) {
    case "cron": {
      const schedule = requiredText(value.trigger.value.schedule, "cron schedule");
      const timezone = requiredText(value.trigger.value.timezone, "cron timezone");
      return { kind: "cron", schedule, timezone };
    }
    case "webhook": {
      const registrationId = requiredText(
        value.trigger.value.registrationId,
        "webhook registration_id",
      );
      const events = [...new Set(value.trigger.value.events.map((event) => event.trim()))];
      if (
        events.length === 0 ||
        events.length > MAX_WEBHOOK_EVENTS ||
        events.some(
          (event) => event.length > MAX_EVENT_KEY_LENGTH || !EVENT_KEY_RE.test(event),
        )
      ) {
        throw new ConnectError(
          `webhook events must contain 1-${MAX_WEBHOOK_EVENTS} lowercase dot-delimited event keys`,
          Code.InvalidArgument,
        );
      }
      const filter = value.trigger.value.filterJson
        ? parseObjectJson(value.trigger.value.filterJson, "webhook filter_json")
        : undefined;
      if (filter) assertWebhookFilter(filter);
      return {
        kind: "webhook",
        registrationId,
        events,
        ...(filter ? { filter } : {}),
      };
    }
    case "integration": {
      const provider = requiredText(value.trigger.value.provider, "integration provider");
      const connectionId = requiredText(
        value.trigger.value.connectionId,
        "integration connection_id",
      );
      const eventKeys = [...new Set(value.trigger.value.eventKeys.map((key) => key.trim()))];
      if (
        eventKeys.length === 0 ||
        eventKeys.length > MAX_WEBHOOK_EVENTS ||
        eventKeys.some((key) => key.length > MAX_EVENT_KEY_LENGTH || !EVENT_KEY_RE.test(key))
      ) {
        throw new ConnectError(
          `integration event_keys must contain 1-${MAX_WEBHOOK_EVENTS} lowercase dot-delimited event keys`,
          Code.InvalidArgument,
        );
      }
      const scopeValues = value.trigger.value.scopeValues.map((v) => v.trim());
      const scopeFromInput = value.trigger.value.scopeFromInput?.trim() || undefined;
      if (scopeValues.length > 0 && scopeFromInput !== undefined) {
        throw new ConnectError(
          "integration scope takes values or an input binding, not both",
          Code.InvalidArgument,
        );
      }
      if (scopeValues.some((v) => v.length === 0)) {
        throw new ConnectError("integration scope values must be non-empty", Code.InvalidArgument);
      }
      const scope =
        scopeValues.length > 0
          ? { values: scopeValues }
          : scopeFromInput !== undefined
            ? { fromInput: scopeFromInput }
            : undefined;
      return {
        kind: "integration",
        provider,
        connectionId,
        eventKeys,
        ...(scope ? { scope } : {}),
      };
    }
    default:
      throw new ConnectError("automation trigger is required", Code.InvalidArgument);
  }
}

/** Optional catalog selections may arrive as ""; absent means "inherit". */
function catalogOptionId(value: string | undefined): string | undefined {
  return value?.trim() || undefined;
}

function parseAction(value: ProtoAutomationAction | undefined): AutomationAction {
  if (value?.action.case !== "createTask") {
    throw new ConnectError("create_task automation action is required", Code.InvalidArgument);
  }
  const action = value.action.value;
  const titleTemplate = action.titleTemplate || undefined;
  const harness = catalogOptionId(action.harness);
  const model = catalogOptionId(action.model);
  const effort = catalogOptionId(action.effort);
  // Presence matters for the router field: absent inherits the profile, while
  // an explicitly empty value selects the direct/native route.
  const modelRouter =
    action.modelRouter === undefined ? undefined : action.modelRouter.trim();
  return {
    kind: "create_task",
    profileId: requiredText(action.profileId, "action profile_id"),
    promptTemplate: action.promptTemplate,
    ...(titleTemplate !== undefined ? { titleTemplate } : {}),
    includeEventContext: action.includeEventContext,
    ...(action.harnessMode ? { harnessMode: action.harnessMode } : {}),
    ...(harness !== undefined ? { harness } : {}),
    ...(model !== undefined ? { model } : {}),
    ...(modelRouter !== undefined ? { modelRouter } : {}),
    ...(effort !== undefined ? { effort } : {}),
  };
}

function protoTrigger(trigger: AutomationTrigger): ProtoAutomationTrigger {
  if (trigger.kind === "cron") {
    return create(AutomationTriggerSchema, {
      trigger: {
        case: "cron",
        value: { schedule: trigger.schedule, timezone: trigger.timezone },
      },
    });
  }
  if (trigger.kind === "integration") {
    return create(AutomationTriggerSchema, {
      trigger: {
        case: "integration",
        value: {
          provider: trigger.provider,
          connectionId: trigger.connectionId,
          eventKeys: trigger.eventKeys,
          ...(trigger.scope && "values" in trigger.scope
            ? { scopeValues: trigger.scope.values }
            : {}),
          ...(trigger.scope && "fromInput" in trigger.scope
            ? { scopeFromInput: trigger.scope.fromInput }
            : {}),
        },
      },
    });
  }
  if (trigger.kind !== "webhook") {
    // Manual triggers arrive with the phase-3 proto; the legacy list/get
    // surface only reconstructs cron/webhook/integration rows.
    throw new ConnectError(
      `trigger kind "${trigger.kind}" has no legacy proto shape`,
      Code.Internal,
    );
  }
  return create(AutomationTriggerSchema, {
    trigger: {
      case: "webhook",
      value: {
        registrationId: trigger.registrationId,
        events: trigger.events,
        ...(trigger.filter ? { filterJson: JSON.stringify(trigger.filter) } : {}),
      },
    },
  });
}

function protoAction(action: AutomationAction): ProtoAutomationAction {
  return create(AutomationActionSchema, {
    action: {
      case: "createTask",
      value: {
        profileId: action.profileId,
        promptTemplate: action.promptTemplate,
        ...(action.titleTemplate !== undefined ? { titleTemplate: action.titleTemplate } : {}),
        includeEventContext: action.includeEventContext,
        ...(action.harnessMode !== undefined ? { harnessMode: action.harnessMode } : {}),
        ...(action.harness !== undefined ? { harness: action.harness } : {}),
        ...(action.model !== undefined ? { model: action.model } : {}),
        ...(action.modelRouter !== undefined ? { modelRouter: action.modelRouter } : {}),
        ...(action.effort !== undefined ? { effort: action.effort } : {}),
      },
    },
  });
}

function toProtoAutomation(row: AutomationRow): ProtoAutomation {
  return create(AutomationSchema, {
    id: row.id,
    name: row.name,
    description: row.description,
    enabled: row.enabled,
    trigger: protoTrigger(row.trigger),
    action: protoAction(row.action),
    ...(row.createdByUserId ? { createdByUserId: row.createdByUserId } : {}),
    ...(row.nextFireAt ? { nextFireAt: row.nextFireAt.toISOString() } : {}),
    ...(row.lastFiredAt ? { lastFiredAt: row.lastFiredAt.toISOString() } : {}),
    archived: row.archivedAt !== null,
    createdAt: row.createdAt.toISOString(),
    updatedAt: row.updatedAt.toISOString(),
  });
}

function toProtoRun(row: AutomationRunRow): ProtoAutomationRun {
  return create(AutomationRunSchema, {
    id: row.id,
    automationId: row.automationId,
    triggerJson: JSON.stringify(row.trigger),
    ...(row.renderedPrompt !== null ? { renderedPrompt: row.renderedPrompt } : {}),
    ...(row.renderedTitle !== null ? { renderedTitle: row.renderedTitle } : {}),
    ...(row.taskId !== null ? { taskId: row.taskId } : {}),
    ...(row.sessionId !== null ? { sessionId: row.sessionId } : {}),
    // Engine statuses mapped onto the strings the current page renders;
    // remove in phase 3 with the proto break.
    status: legacyRunStatus(row.status),
    ...(row.error !== null ? { error: row.error } : {}),
    ...(row.scheduledFor ? { scheduledFor: row.scheduledFor.toISOString() } : {}),
    createdAt: row.createdAt.toISOString(),
  });
}

function toProtoRegistration(row: WebhookRegistrationRow): ProtoWebhookRegistration {
  return create(WebhookRegistrationSchema, {
    id: row.id,
    name: row.name,
    verificationScheme: row.verification.scheme,
    ...(row.providerHint ? { providerHint: row.providerHint } : {}),
    ...(row.createdByUserId ? { createdByUserId: row.createdByUserId } : {}),
    createdAt: row.createdAt.toISOString(),
    updatedAt: row.updatedAt.toISOString(),
  });
}

function toProtoSample(row: WebhookSampleRow): ProtoWebhookSample {
  return create(WebhookSampleSchema, {
    id: row.id,
    registrationId: row.registrationId,
    eventKey: row.eventKey,
    payloadJson: JSON.stringify(row.payload),
    receivedAt: row.receivedAt.toISOString(),
  });
}

function templateError(field: string, error: unknown): {
  field: string;
  code: string;
  message: string;
} {
  if (error instanceof AutomationTemplateError) {
    return { field, code: error.code, message: error.message };
  }
  return {
    field,
    code: "render_failed",
    message: error instanceof Error ? error.message : String(error),
  };
}

async function facetAliases(
  registrationId: string,
  store: AutomationStore,
  connectors: CustomConnectorSource,
): Promise<WebhookAliasSpec[]> {
  if (!registrationId) return [];
  const registration = await store.getRegistration(registrationId);
  if (!registration?.providerHint) return [];
  const registry = await loadRegistry(connectors);
  return registry.get(registration.providerHint)?.webhook?.aliases ?? [];
}

export function registerAutomations(router: ConnectRouter, deps?: AutomationDeps): void {
  const getSession = deps?.getSession ?? getSessionFromHeaders;
  const store = deps?.store ?? makeAutomationStore(getDb());
  const profiles = deps?.profiles ?? makeProfileStore(getDb());
  const connectors = deps?.connectors ?? { list: () => makeConnectorStore(getDb()).list() };
  const harnessCatalog: HarnessCatalogClient =
    deps?.harnessCatalog ?? (defaultHarnessCatalog as unknown as HarnessCatalogClient);
  // Keep router persistence lazy so non-routed automation requests and
  // dependency-injected tests do not open the production database.
  const modelRouters = (): ModelRouterStore =>
    deps?.modelRouters ?? makeModelRouterStore(getDb());
  const integrationEvents = (): Pick<
    IntegrationEventStore,
    "getLatest" | "listObservedEventKeys"
  > => deps?.integrationEvents ?? makeIntegrationEventStore(getDb());
  const connections = (): Pick<IntegrationConnectionStore, "getDefault"> =>
    deps?.connections ?? makeIntegrationConnectionStore(getDb());
  const eventSample = deps?.eventSample ?? loadEventSample;
  const now = deps?.now ?? (() => new Date());
  const evalCode = deps?.evalCode ?? evaluateCode;
  const rateLimiter = makeEvalRateLimiter();

  /** Resolve a provider that declares inbound events, or InvalidArgument. */
  async function connectorWithWebhookFacet(
    provider: string,
  ): Promise<{ connector: Connector; webhook: NonNullable<Connector["webhook"]> }> {
    const connector = (await loadRegistry(connectors)).get(provider);
    const webhook = connector?.webhook;
    if (!connector || !webhook) {
      throw new ConnectError(
        `provider "${provider}" has no webhook event catalog`,
        Code.InvalidArgument,
      );
    }
    return { connector, webhook };
  }

  const randomSecret =
    deps?.randomSecret ??
    (() => Buffer.from(crypto.getRandomValues(new Uint8Array(32))).toString("base64url"));
  const orgSecret: OrgSecretClient =
    deps?.orgSecret ?? {
      putSecret: (req) => defaultOrgSecret.putSecret(req),
      deleteSecret: (req) => defaultOrgSecret.deleteSecret(req),
    };

  /**
   * ADR 0063 B2: validate the automation's harness/model/effort override against
   * the live catalog, returning the action with the harness those option ids
   * belong to PINNED onto it.
   *
   * A model/effort id is only meaningful next to one harness, so an action that
   * names a model but inherits its harness is under-specified: an admin who
   * later switches the profile's harness would orphan the stored id, and the
   * launch path resolves an unknown id to nothing (compileSessionCreateInput
   * sets no model env), silently running the new harness's default months later.
   * A profile can't drift this way because ProfileService validates its whole
   * triple on every save; pinning gives the action the same property instead of
   * a second guard in ProfileService that must stay in sync forever. Clearing
   * the harness in the editor clears model/effort with it, so "follow the
   * profile" stays reachable — it just cannot mean "keep a foreign model id".
   *
   * The catalog is read only when the automation overrides something; the
   * profile's own selection was already validated by ProfileService.
   */
  async function resolveOverride(
    action: CreateTaskAutomationAction,
    profileHarness: string,
  ): Promise<CreateTaskAutomationAction> {
    if (
      action.harness === undefined
      && action.model === undefined
      && action.modelRouter === undefined
      && action.effort === undefined
    ) {
      return action;
    }
    const harness = action.harness ?? profileHarness;
    const { harnesses } = await harnessCatalog.listHarnesses({});
    const descriptor = harnesses.find((h) => h.name === harness)?.descriptor;
    if (!descriptor) {
      throw new ConnectError(`harness "${harness}" is not in the catalog`, Code.InvalidArgument);
    }
    if (action.modelRouter) {
      const router = getModelRouterDefinition(action.modelRouter);
      if (!router) throw new ConnectError(`model router "${action.modelRouter}" is not registered`, Code.InvalidArgument);
      if (!selectRouterProtocol(router, descriptor.routerProtocols ?? [])) {
        throw new ConnectError(`harness "${harness}" does not support model router "${router.id}"`, Code.InvalidArgument);
      }
      if (action.model !== undefined && !(await modelRouters().getModel(router.id, action.model))) {
        throw new ConnectError(`model "${action.model}" is not in router "${router.id}"`, Code.InvalidArgument);
      }
    } else if (action.model !== undefined && !(descriptor.models ?? []).some((m) => m.id === action.model)) {
      throw new ConnectError(
        `model "${action.model}" is not valid for harness "${harness}"`,
        Code.InvalidArgument,
      );
    }
    if (action.effort !== undefined && !(descriptor.effort ?? []).some((e) => e.id === action.effort)) {
      throw new ConnectError(
        `effort "${action.effort}" is not valid for harness "${harness}"`,
        Code.InvalidArgument,
      );
    }
    return { ...action, harness };
  }

  async function validateInput(input: {
    name: string;
    description: string;
    enabled: boolean;
    trigger?: ProtoAutomationTrigger;
    action?: ProtoAutomationAction;
  }): Promise<AutomationInput> {
    const name = requiredText(input.name, "name");
    const trigger = parseTrigger(input.trigger);
    const parsed = parseAction(input.action);

    const profile = await profiles.getActive(parsed.profileId);
    if (!profile) {
      throw new ConnectError("action profile_id is not an active profile", Code.InvalidArgument);
    }
    // A profile's port_exposures are ignored on the automation path, not a
    // reason to reject the profile: an automation session has no user owner,
    // and `port_exposure.owner_user_id` is NOT NULL. Only
    // `createTaskWithSession` auto-mints; `createSessionForExistingTask` (the
    // automation path) never does. An admin can still expose a port by hand on
    // a live automation session via POST /api/v1/sessions/:id/ports.
    const action = await resolveOverride(parsed, profile.harness);

    try {
      validateAutomationTemplate(action.promptTemplate);
      if (action.titleTemplate !== undefined) validateAutomationTemplate(action.titleTemplate);
    } catch (error) {
      throw new ConnectError(
        error instanceof Error ? error.message : String(error),
        Code.InvalidArgument,
      );
    }

    let nextFireAt: Date | null = null;
    if (trigger.kind === "cron") {
      nextFireAt = nextCronFire(trigger.schedule, trigger.timezone, now());
    } else if (
      trigger.kind === "webhook"
      && trigger.registrationId !== SYSTEM_GITHUB_REGISTRATION_ID
      && !(await store.getRegistration(trigger.registrationId))
    ) {
      throw new ConnectError("webhook registration not found", Code.InvalidArgument);
    } else if (trigger.kind === "integration") {
      const { webhook } = await connectorWithWebhookFacet(trigger.provider);
      const declared = new Set(webhook.events.map((event) => event.key));
      const unknown = trigger.eventKeys.filter((key) => !declared.has(key));
      if (unknown.length > 0) {
        throw new ConnectError(
          `provider "${trigger.provider}" does not declare event keys: ${unknown.join(", ")}`,
          Code.InvalidArgument,
        );
      }
    }

    return {
      name,
      description: input.description.trim(),
      enabled: input.enabled,
      trigger,
      action,
      nextFireAt,
    };
  }

  router.service(AutomationService, {
    async createAutomation(req, ctx) {
      const userId = (await requireAdmin(ctx, getSession)).id;
      const input = await validateInput(req);
      return { automation: toProtoAutomation(await store.create(input, userId)) };
    },

    async updateAutomation(req, ctx) {
      await requireAdmin(ctx, getSession);
      const id = requiredText(req.id, "id");
      const input = await validateInput(req);
      const row = await store.update(id, input);
      if (!row) throw new ConnectError("automation not found", Code.NotFound);
      return { automation: toProtoAutomation(row) };
    },

    async archiveAutomation(req, ctx) {
      await requireAdmin(ctx, getSession);
      const row = await store.archive(requiredText(req.id, "id"));
      if (!row) throw new ConnectError("automation not found", Code.NotFound);
      return { automation: toProtoAutomation(row) };
    },

    async getAutomation(req, ctx) {
      await requireAdmin(ctx, getSession);
      const row = await store.get(requiredText(req.id, "id"));
      if (!row) throw new ConnectError("automation not found", Code.NotFound);
      return { automation: toProtoAutomation(row) };
    },

    async listAutomations(req, ctx) {
      await requireAdmin(ctx, getSession);
      return {
        automations: (await store.list({ includeArchived: req.includeArchived })).map(
          toProtoAutomation,
        ),
      };
    },

    async setAutomationEnabled(req, ctx) {
      await requireAdmin(ctx, getSession);
      const id = requiredText(req.id, "id");
      const existing = await store.getActive(id);
      if (!existing) throw new ConnectError("automation not found", Code.NotFound);
      const nextFireAt =
        req.enabled && existing.trigger.kind === "cron"
          ? nextCronFire(existing.trigger.schedule, existing.trigger.timezone, now())
          : undefined;
      const row = await store.setEnabled(id, req.enabled, nextFireAt);
      if (!row) throw new ConnectError("automation not found", Code.NotFound);
      return { automation: toProtoAutomation(row) };
    },

    async listAutomationRuns(req, ctx) {
      await requireAdmin(ctx, getSession);
      const automationId = requiredText(req.automationId, "automation_id");
      if (!(await store.get(automationId))) {
        throw new ConnectError("automation not found", Code.NotFound);
      }
      return {
        runs: (await store.listRuns(automationId, listLimit(req.limit))).map(toProtoRun),
      };
    },

    async listWebhookSamples(req, ctx) {
      await requireAdmin(ctx, getSession);
      const registrationId = requiredText(req.registrationId, "registration_id");
      if (!(await store.getRegistration(registrationId))) {
        throw new ConnectError("webhook registration not found", Code.NotFound);
      }
      return {
        samples: (
          await store.listSamples(
            registrationId,
            req.eventKey || undefined,
            listLimit(req.limit),
          )
        ).map(toProtoSample),
      };
    },

    async testRender(req, ctx) {
      await requireAdmin(ctx, getSession);
      const stored = req.automationId ? await store.get(req.automationId) : null;
      if (req.automationId && !stored) {
        throw new ConnectError("automation not found", Code.NotFound);
      }

      let action: CreateTaskAutomationAction;
      try {
        const candidate = req.draftAction ? parseAction(req.draftAction) : stored?.action;
        if (!candidate) {
          throw new ConnectError(
            "automation_id or draft_action is required",
            Code.InvalidArgument,
          );
        }
        action = candidate;
      } catch (error) {
        if (error instanceof ConnectError) throw error;
        throw new ConnectError(String(error), Code.InvalidArgument);
      }

      let sample: WebhookSampleRow | null = null;
      let rawPayload: Record<string, unknown> = {};
      if (req.sample.case === "sampleId") {
        sample = await store.getSample(req.sample.value);
        if (!sample) {
          return {
            errors: [
              { field: "sample", code: "invalid_sample", message: "sample not found" },
            ],
          };
        }
        const expectedRegistrationId =
          req.registrationId ||
          (stored?.trigger.kind === "webhook" ? stored.trigger.registrationId : "");
        if (expectedRegistrationId && sample.registrationId !== expectedRegistrationId) {
          return {
            errors: [
              {
                field: "sample",
                code: "invalid_sample",
                message: "sample belongs to a different webhook registration",
              },
            ],
          };
        }
        rawPayload = sample.payload;
      } else if (req.sample.case === "payloadJson") {
        try {
          rawPayload = parseObjectJson(req.sample.value, "payload_json");
        } catch (error) {
          return {
            errors: [
              {
                field: "sample",
                code: "invalid_sample",
                message: error instanceof Error ? error.message : String(error),
              },
            ],
          };
        }
      }

      const storedWebhook = stored?.trigger.kind === "webhook" ? stored.trigger : undefined;
      const registrationId =
        req.registrationId || sample?.registrationId || storedWebhook?.registrationId || "";
      let eventKey = req.eventKey || sample?.eventKey || "";
      if (!sample && req.sample.case === undefined && registrationId) {
        const latest = await store.getLatestSample(registrationId, eventKey || undefined);
        if (latest) {
          sample = latest;
          rawPayload = latest.payload;
          eventKey ||= latest.eventKey;
        }
      }

      const isCron = req.scheduledFor !== undefined || stored?.trigger.kind === "cron";
      if (
        !isCron &&
        registrationId &&
        req.sample.case === undefined &&
        sample === null
      ) {
        return {
          errors: [
            {
              field: "sample",
              code: "invalid_sample",
              message: "no stored sample is available for this webhook registration",
            },
          ],
        };
      }
      const aliases = isCron ? [] : await facetAliases(registrationId, store, connectors);
      const context = buildAutomationTemplateContext({
        automationName: req.automationName ?? stored?.name ?? "Draft automation",
        triggerKind: isCron ? "cron" : "webhook",
        receivedAt: (sample?.receivedAt ?? now()).toISOString(),
        ...(eventKey ? { eventKey } : {}),
        ...(req.scheduledFor !== undefined ? { scheduledFor: req.scheduledFor } : {}),
        rawPayload,
        aliases,
      });

      let renderedPrompt: string;
      try {
        renderedPrompt = (
          await renderAutomationAction(
            { ...action, titleTemplate: undefined },
            context,
          )
        ).prompt;
      } catch (error) {
        return { errors: [templateError("prompt_template", error)] };
      }

      let renderedTitle: string | undefined;
      if (action.titleTemplate !== undefined) {
        try {
          renderedTitle = await renderAutomationTemplate(action.titleTemplate, context);
        } catch (error) {
          return { errors: [templateError("title_template", error)] };
        }
      }
      return {
        renderedPrompt,
        ...(renderedTitle !== undefined ? { renderedTitle } : {}),
        errors: [],
      };
    },

    // ADR 0119 D6: the editor's Run button. Admin-gated like the rest of the
    // service (the design doc floated member-callable; the whole page is
    // admin-only today, so the wider gate waits for phase 3). Rate-limited —
    // it is an arbitrary-compute endpoint, even caged.
    async evalCode(req, ctx) {
      const userId = (await requireAdmin(ctx, getSession)).id;
      if (!rateLimiter.allow(userId, now().getTime())) {
        throw new ConnectError(
          `rate limit: at most ${EVAL_CODE_LIMIT_PER_MINUTE} evaluations per minute`,
          Code.ResourceExhausted,
        );
      }
      const source = requiredText(req.source, "source");
      if (source.length > CODE_SOURCE_MAX_CHARS) {
        throw new ConnectError(
          `source is ${source.length} characters (max ${CODE_SOURCE_MAX_CHARS})`,
          Code.InvalidArgument,
        );
      }
      if (req.mode !== "value" && req.mode !== "boolean") {
        throw new ConnectError(`mode must be "value" or "boolean"`, Code.InvalidArgument);
      }
      if (req.inputJson.length > EVAL_CODE_INPUT_MAX_CHARS) {
        throw new ConnectError(
          `input_json is ${req.inputJson.length} characters (max ${EVAL_CODE_INPUT_MAX_CHARS})`,
          Code.InvalidArgument,
        );
      }
      let input: CodeInput = {};
      if (req.inputJson !== "") {
        let parsed: unknown;
        try {
          parsed = JSON.parse(req.inputJson);
        } catch {
          throw new ConnectError("input_json is not valid JSON", Code.InvalidArgument);
        }
        if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
          throw new ConnectError("input_json must be a JSON object", Code.InvalidArgument);
        }
        const record = parsed as Record<string, unknown>;
        input = {
          ...(record["event"] !== undefined ? { event: record["event"] } : {}),
          ...(record["steps"] !== undefined ? { steps: record["steps"] } : {}),
          ...(record["inputs"] !== undefined ? { inputs: record["inputs"] } : {}),
          ...(record["trigger"] !== undefined ? { trigger: record["trigger"] } : {}),
        };
      }
      const outcome = await evalCode(source, input, req.mode);
      if (outcome.ok) {
        return {
          valueJson: JSON.stringify(outcome.value),
          logs: outcome.logs,
          durationMs: BigInt(outcome.durationMs),
        };
      }
      return {
        errorName: outcome.error.name,
        errorMessage: outcome.error.message,
        ...(outcome.error.line !== undefined ? { errorLine: outcome.error.line } : {}),
        logs: outcome.logs,
        durationMs: BigInt(outcome.durationMs),
      };
    },
    // Admin-gated like the rest of this service; the plan's member-readable
    // posture arrives with the phase-3 surface (the whole page is admin).
    async listEventCatalog(req, ctx) {
      await requireAdmin(ctx, getSession);
      const provider = requiredText(req.provider, "provider");
      const { webhook: facet } = await connectorWithWebhookFacet(provider);
      const connection = await connections().getDefault(provider);
      const observed = new Set(
        connection ? await integrationEvents().listObservedEventKeys(connection.id) : [],
      );

      const events = [];
      for (const event of facet.events) {
        let sampleJson = "";
        if (connection) {
          const latest = await integrationEvents().getLatest(connection.id, event.key);
          if (latest) sampleJson = JSON.stringify(latest.payload);
        }
        if (sampleJson === "") {
          const fixture = eventSample(provider, event.key);
          if (fixture) sampleJson = JSON.stringify(fixture);
        }
        events.push({
          key: event.key,
          label: event.label,
          description: event.description ?? "",
          schemaJson: event.schema ? JSON.stringify(event.schema) : "",
          sampleJson,
          observed: observed.has(event.key),
          hidden: event.hidden === true,
        });
      }
      return {
        events,
        ...(facet.scope ? { scope: { key: facet.scope.key, label: facet.scope.label } } : {}),
        variables: facet.aliases,
        defaultConnectionId: connection?.id ?? "",
      };
    },

    async listActionCatalog(req, ctx) {
      await requireAdmin(ctx, getSession);
      const provider = requiredText(req.provider, "provider");
      const connector = (await loadRegistry(connectors)).get(provider);
      if (!connector) {
        throw new ConnectError(`provider "${provider}" is not a connector`, Code.InvalidArgument);
      }
      // Member-safe projection: id/label/description/input schema only —
      // never the execution details (buildProviderCatalog precedent).
      return {
        actions: (connector.actions ?? []).map((action) => ({
          id: action.id,
          label: action.label,
          description: action.description ?? "",
          inputSchemaJson: JSON.stringify(action.inputSchema),
        })),
      };
    },
  });

  router.service(WebhookRegistrationService, {
    async createWebhookRegistration(req, ctx) {
      const userId = (await requireAdmin(ctx, getSession)).id;
      const id = requiredText(req.id, "id");
      if (!REGISTRATION_ID_RE.test(id)) {
        throw new ConnectError(
          "id must be a lowercase URL-safe slug of at most 63 characters",
          Code.InvalidArgument,
        );
      }
      if (await store.getRegistration(id)) {
        throw new ConnectError("webhook registration already exists", Code.AlreadyExists);
      }
      const scheme = verificationScheme(req.verificationScheme);
      const providerHint = req.providerHint?.trim() || null;
      if (providerHint) {
        const connector = (await loadRegistry(connectors)).get(providerHint);
        if (!connector?.webhook) {
          throw new ConnectError(
            "provider_hint does not name a connector with a webhook facet",
            Code.InvalidArgument,
          );
        }
        if (connector.webhook.verificationScheme !== scheme) {
          throw new ConnectError(
            `verification_scheme must be ${connector.webhook.verificationScheme} for ${providerHint}`,
            Code.InvalidArgument,
          );
        }
      }

      const secretRef = `webhook.${id}.secret`;
      const secret = randomSecret();
      const registration = await store.createRegistration({
        id,
        name: requiredText(req.name, "name"),
        verification: { scheme, secretRef },
        providerHint,
        createdByUserId: userId,
      });
      try {
        await orgSecret.putSecret({ name: secretRef, value: secret });
      } catch (error) {
        await store.deleteRegistration(id);
        throw new ConnectError(
          `failed to provision webhook secret: ${error instanceof Error ? error.message : String(error)}`,
          Code.Internal,
        );
      }
      return { registration: toProtoRegistration(registration), secret };
    },

    async listWebhookRegistrations(_req, ctx) {
      await requireAdmin(ctx, getSession);
      return {
        registrations: (await store.listRegistrations()).map(toProtoRegistration),
      };
    },

    async deleteWebhookRegistration(req, ctx) {
      await requireAdmin(ctx, getSession);
      const id = requiredText(req.id, "id");
      const boundAutomations = await store.listBoundToWebhookRegistration(id);
      if (boundAutomations.length > 0) {
        throw new ConnectError(
          `cannot delete webhook registration "${id}": ${boundAutomations.length} non-archived automation(s) reference it (${boundAutomations.map((automation) => automation.id).join(", ")})`,
          Code.FailedPrecondition,
        );
      }
      const deleted = await store.deleteRegistration(id);
      // Always attempt the deterministic secret key so retrying after a partial
      // failure cleans up the sealed value even when the PG row is already gone.
      await orgSecret.deleteSecret({ name: `webhook.${id}.secret` });
      return { deleted };
    },

    async listWebhookEvents(req, ctx) {
      await requireAdmin(ctx, getSession);
      const registration = await store.getRegistration(
        requiredText(req.registrationId, "registration_id"),
      );
      if (!registration) {
        throw new ConnectError("webhook registration not found", Code.NotFound);
      }
      const observed = new Set(await store.listObservedEventKeys(registration.id));
      const facet = registration.providerHint
        ? (await loadRegistry(connectors)).get(registration.providerHint)?.webhook
        : undefined;
      const events = new Map(
        (facet?.events ?? [])
          // hidden events reach the ledger but never the trigger picker
          // (installation.* lifecycle noise).
          .filter((event) => event.hidden !== true)
          .map((event) => [
            event.key,
            {
              key: event.key,
              displayName: event.label,
              observed: observed.has(event.key),
            },
          ]),
      );
      for (const key of observed) {
        if (!events.has(key)) events.set(key, { key, displayName: key, observed: true });
      }
      return {
        events: [...events.values()].sort((a, b) => a.key.localeCompare(b.key)),
        variables: facet?.aliases ?? [],
      };
    },
  });
}
