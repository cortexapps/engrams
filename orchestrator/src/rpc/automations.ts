/** AutomationService / AutomationRunService v2 (ADR 0119 phase 3).
 *
 * Definitions are data; the orchestrator validates and answers with
 * block-addressed errors. Built-ins are structure-locked: SaveVersion and
 * settings changes are refused, while inputs and tunable block overrides are
 * the per-org edit surface (the ADR 0119 built-in editing model).
 */

import { create } from "@bufbuild/protobuf";
import { ConnectError, Code } from "@connectrpc/connect";
import type { ConnectRouter } from "@connectrpc/connect";
import { Cron } from "croner";

import {
  AutomationRunService,
  AutomationSchema,
  AutomationService,
  WebhookRegistrationSchema,
  WebhookRegistrationService,
  type Automation as ProtoAutomation,
  type AutomationRunBrief as ProtoRunBrief,
  type AutomationStepRun as ProtoStepRun,
  type AutomationVersion as ProtoVersion,
  type BlockError as ProtoBlockError,
  type DayRunCount as ProtoDayRunCount,
  type WebhookRegistration as ProtoWebhookRegistration,
} from "../gen/engram/app/v1/automation_pb.ts";
import { getSessionFromHeaders } from "../auth/session.ts";
import { requireAdmin } from "./require.ts";
import { evaluateCode, type CodeInput } from "../automations/code/sandbox.ts";
import { CODE_SOURCE_MAX_CHARS } from "../automations/engine/blocks/code.ts";
import {
  definitionOf,
  effectiveDefinition,
  makeAutomationStore,
  resolveAutomationInputs,
  type AutomationRow,
  type AutomationRunRow,
  type AutomationStepRunRow,
  type AutomationStore,
  type AutomationVersionRow,
  type DayRunCount,
  type WebhookRegistrationRow,
} from "../db/automations.ts";
import { getDb } from "../db/client.ts";
import { inputErrorField, validateInputValues } from "../automations/inputs.ts";
import { makeConnectorStore } from "../db/connectors.ts";
import { makeProfileStore, type ProfileStore } from "../db/profiles.ts";
import { makeModelRouterStore, type ModelRouterStore } from "../db/model-routers.ts";
import { getModelRouterDefinition, selectRouterProtocol } from "../model-routers/registry.ts";
import { harnessCatalog as defaultHarnessCatalog } from "../control-plane/client.ts";
import type { HarnessCatalogClient } from "./task-create.ts";
import type { WebhookVerificationScheme } from "../db/schema.ts";
import {
  type Connector,
  type CustomConnectorSource,
  loadRegistry,
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
import { orgSecret as defaultOrgSecret } from "../control-plane/client.ts";
import {
  applyBlockOverrides,
  BlockOverrideError,
  DefinitionError,
  settingsSchema,
  validateDefinition,
  type AutomationDefinition,
  type AutomationSettings,
  type BlockOverrides,
  type InputFieldSpec,
} from "../automations/engine/definition.ts";
import { previewDefinition } from "../automations/engine/preview.ts";
import { makeWebhookAliasResolver } from "../automations/aliases.ts";
import {
  admitAutomationRun,
  automationRunId,
  defaultWorkflowStarter,
  type AutomationWebhookStarter,
} from "../automations/dispatch.ts";
import {
  defaultAutomationSender,
  inboxKeys,
  type AutomationSender,
} from "../automations/engine/inbox.ts";
import type { AutomationDispatchStore } from "../db/automations.ts";
import { getSlackClient } from "../integrations/slack.ts";
import { makeLinearIssueClient } from "../integrations/linear-issues.ts";
import type { WebhookAliasMapping } from "../automations/template.ts";

export type GetSession = (
  headers: Headers,
) => Promise<{ user: { id: string; role?: string | null } } | null>;

export interface OrgSecretClient {
  putSecret(req: { name: string; value: string }): Promise<unknown>;
  deleteSecret(req: { name: string }): Promise<{ deleted: boolean }>;
}

export interface InputKeyOptionSource {
  list(noun: string, connectionId: string | undefined): Promise<Array<{ key: string; label: string }>>;
}

export interface AutomationDeps {
  getSession?: GetSession;
  store?: AutomationStore & AutomationDispatchStore;
  profiles?: Pick<ProfileStore, "getActive">;
  harnessCatalog?: HarnessCatalogClient;
  modelRouters?: ModelRouterStore;
  connectors?: CustomConnectorSource;
  orgSecret?: OrgSecretClient;
  integrationEvents?: Pick<
    IntegrationEventStore,
    "getLatest" | "listObservedEventKeys" | "list" | "getById" | "listObservedScopeValues"
  >;
  connections?: Pick<IntegrationConnectionStore, "getDefault">;
  eventSample?: typeof loadEventSample;
  aliases?: (registrationId: string) => Promise<readonly WebhookAliasMapping[]>;
  workflowStarter?: AutomationWebhookStarter;
  sender?: AutomationSender;
  inputKeyOptions?: InputKeyOptionSource;
  now?: () => Date;
  randomId?: () => string;
  randomSecret?: () => string;
  /** Test seam for the QuickJS sandbox behind EvalCode. */
  evalCode?: typeof evaluateCode;
}

export const EVAL_CODE_LIMIT_PER_MINUTE = 30;
export const EVAL_CODE_INPUT_MAX_CHARS = 512 * 1024;
export const DEFINITION_MAX_CHARS = 512 * 1024;
export const INPUTS_MAX_CHARS = 256 * 1024;

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
const DEFAULT_LIST_LIMIT = 50;
const MAX_LIST_LIMIT = 200;

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

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

function parseJson(value: string, field: string, maxChars: number): unknown {
  if (value.length > maxChars) {
    throw new ConnectError(
      `${field} is ${value.length} characters (max ${maxChars})`,
      Code.InvalidArgument,
    );
  }
  try {
    return JSON.parse(value);
  } catch (error) {
    throw new ConnectError(
      `${field} is not valid JSON: ${error instanceof Error ? error.message : String(error)}`,
      Code.InvalidArgument,
    );
  }
}

function parseObjectJson(value: string, field: string, maxChars = INPUTS_MAX_CHARS): Record<string, unknown> {
  const parsed = parseJson(value, field, maxChars);
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    throw new ConnectError(`${field} must be a JSON object`, Code.InvalidArgument);
  }
  return parsed as Record<string, unknown>;
}

/** A validation failure the editor can route to a block + field. The Connect
 * error carries the structured BlockError in its details-compatible message
 * AND the response-level list where the RPC has one. */
export class BlockValidationError extends ConnectError {
  constructor(readonly blockErrors: ProtoBlockError[]) {
    super(
      blockErrors.map((e) => (e.blockId ? `${e.blockId}.${e.field}: ${e.message}` : `${e.field}: ${e.message}`)).join("; "),
      Code.InvalidArgument,
    );
  }
}

function blockError(blockId: string, field: string, code: string, message: string): ProtoBlockError {
  return { $typeName: "engram.app.v1.BlockError", blockId, field, code, message };
}

function toBlockError(error: unknown): ProtoBlockError | null {
  if (error instanceof DefinitionError) {
    return blockError(error.blockId ?? "", error.field, "invalid_definition", error.message);
  }
  if (error instanceof BlockOverrideError) {
    return blockError(error.blockId, error.field, "invalid_override", error.message);
  }
  return null;
}

/** Reject input VALUES the version's schema does not accept, one routed line
 * per violation (`inputs.<key>[.path]: message` — the form the web's
 * inputErrorFromServer maps back to a field). Undeclared keys are checked by
 * the caller first; this is the value half (ADR 0119 phase 4.3b). */
function assertInputValues(
  schema: readonly InputFieldSpec[],
  inputs: Record<string, unknown>,
): void {
  const errors = validateInputValues(schema, inputs);
  if (errors.length > 0) {
    throw new BlockValidationError(
      errors.map((e) => blockError("", inputErrorField(e), `invalid_input_${e.code}`, e.message)),
    );
  }
}

function verificationScheme(value: string): WebhookVerificationScheme {
  switch (value) {
    case "generic_hmac_sha256":
      return value;
    case "github_hmac_sha256":
    case "slack_v0":
      throw new ConnectError(
        `verification_scheme ${value} is retired; create an integration trigger for the provider, or a generic webhook`,
        Code.InvalidArgument,
      );
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
    throw new BlockValidationError([
      blockError(
        "",
        "trigger.schedule",
        "invalid_cron",
        `invalid cron schedule: ${error instanceof Error ? error.message : String(error)}`,
      ),
    ]);
  }
}

/** Human trigger summary for the list row. */
export function triggerSummary(definition: AutomationDefinition): string {
  const t = definition.trigger;
  switch (t.kind) {
    case "cron":
      return `Cron · ${t.schedule} ${t.timezone}`;
    case "webhook":
      return `Webhook · ${t.registrationId} · ${t.events.join(", ")}`;
    case "integration": {
      const provider = t.provider.charAt(0).toUpperCase() + t.provider.slice(1);
      const events = t.eventKeys.join(", ");
      const scope =
        t.scope === undefined
          ? ""
          : "values" in t.scope
            ? ` · ${t.scope.values.length} scoped`
            : ` · scope from input ${t.scope.fromInput}`;
      return `${provider} · ${events}${scope}`;
    }
    case "manual":
      return "Manual";
  }
}

// ---------------------------------------------------------------------------
// Proto mappers
// ---------------------------------------------------------------------------

function toProtoVersion(row: AutomationVersionRow): ProtoVersion {
  return {
    $typeName: "engram.app.v1.AutomationVersion",
    automationId: row.automationId,
    number: row.version,
    definitionJson: JSON.stringify(definitionOf(row)),
    ...(row.createdByUserId ? { createdByUserId: row.createdByUserId } : {}),
    createdAt: row.createdAt.toISOString(),
  };
}

function toProtoAutomation(row: AutomationRow): ProtoAutomation {
  return create(AutomationSchema, {
    id: row.id,
    name: row.name,
    description: row.description,
    kind: row.kind,
    ...(row.builtinKey ? { builtinKey: row.builtinKey } : {}),
    enabled: row.enabled,
    currentVersion: row.currentVersion,
    inputsJson: JSON.stringify(row.inputs),
    blockOverridesJson: JSON.stringify(row.blockOverrides),
    archived: row.archivedAt !== null,
    ...(row.createdByUserId ? { createdByUserId: row.createdByUserId } : {}),
    ...(row.nextFireAt ? { nextFireAt: row.nextFireAt.toISOString() } : {}),
    ...(row.lastFiredAt ? { lastFiredAt: row.lastFiredAt.toISOString() } : {}),
    createdAt: row.createdAt.toISOString(),
    updatedAt: row.updatedAt.toISOString(),
    version: toProtoVersion(row.version),
  });
}

function toProtoRunBrief(row: AutomationRunRow): ProtoRunBrief {
  return {
    $typeName: "engram.app.v1.AutomationRunBrief",
    id: row.id,
    automationId: row.automationId,
    version: row.version,
    status: row.status,
    ...(row.error !== null ? { error: row.error } : {}),
    triggerSource: row.trigger.source,
    ...(row.trigger.eventKey !== undefined ? { eventKey: row.trigger.eventKey } : {}),
    ...(row.deliveryKey !== null ? { deliveryKey: row.deliveryKey } : {}),
    dryRun: row.dryRun,
    ...(row.startedAt ? { startedAt: row.startedAt.toISOString() } : {}),
    ...(row.endedAt ? { endedAt: row.endedAt.toISOString() } : {}),
    createdAt: row.createdAt.toISOString(),
  };
}

function toProtoStepRun(row: AutomationStepRunRow, sessionId: string | undefined): ProtoStepRun {
  return {
    $typeName: "engram.app.v1.AutomationStepRun",
    blockId: row.blockId,
    attempt: row.attempt,
    status: row.status,
    inputsJson: row.inputs ? JSON.stringify(row.inputs) : "",
    outputsJson: row.outputs ? JSON.stringify(row.outputs) : "",
    ...(row.error !== null ? { error: row.error } : {}),
    ...(sessionId !== undefined ? { sessionId } : {}),
    startedAt: row.startedAt.toISOString(),
    ...(row.endedAt ? { endedAt: row.endedAt.toISOString() } : {}),
  };
}

function toProtoDayCount(row: DayRunCount): ProtoDayRunCount {
  return { $typeName: "engram.app.v1.DayRunCount", ...row };
}

function toProtoRegistration(row: WebhookRegistrationRow): ProtoWebhookRegistration {
  return create(WebhookRegistrationSchema, {
    id: row.id,
    name: row.name,
    verificationScheme: row.verification.scheme,
    ...(row.providerHint ? { providerHint: row.providerHint } : {}),
    ...(row.disabledReason !== null ? { disabledReason: row.disabledReason } : {}),
    ...(row.createdByUserId ? { createdByUserId: row.createdByUserId } : {}),
    createdAt: row.createdAt.toISOString(),
    updatedAt: row.updatedAt.toISOString(),
  });
}

// ---------------------------------------------------------------------------
// Production input-key options
// ---------------------------------------------------------------------------

function productionInputKeyOptions(
  integrationEvents: () => Pick<IntegrationEventStore, "listObservedScopeValues">,
  connections: () => Pick<IntegrationConnectionStore, "getDefault">,
): InputKeyOptionSource {
  return {
    async list(noun, connectionId) {
      switch (noun) {
        case "repository": {
          // Repositories observed on the GitHub connection's ledger: the
          // installation-repos capability is not exposed to the orchestrator.
          const connection = connectionId
            ? { id: connectionId }
            : await connections().getDefault("github");
          if (!connection) return [];
          const repos = await integrationEvents().listObservedScopeValues(connection.id);
          return repos.map((repo) => ({ key: repo, label: repo }));
        }
        case "channel": {
          const client = await getSlackClient();
          const result = await client.conversations.list({ limit: 200, exclude_archived: true });
          return (result.channels ?? [])
            .filter((c) => typeof c.id === "string")
            .map((c) => ({ key: c.id!, label: c.name ? `#${c.name}` : c.id! }));
        }
        case "team": {
          const workspace = await makeLinearIssueClient().readWorkspace();
          return workspace.teams.map((team) => ({ key: team.id, label: team.name }));
        }
        default:
          throw new ConnectError(`unknown input noun "${noun}"`, Code.InvalidArgument);
      }
    },
  };
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

export function registerAutomations(router: ConnectRouter, deps?: AutomationDeps): void {
  const getSession = deps?.getSession ?? getSessionFromHeaders;
  const store = deps?.store ?? makeAutomationStore(getDb());
  const profiles = () => deps?.profiles ?? makeProfileStore(getDb());
  const harnessCatalog = (): HarnessCatalogClient =>
    deps?.harnessCatalog ?? (defaultHarnessCatalog as unknown as HarnessCatalogClient);
  const modelRouters = (): ModelRouterStore => deps?.modelRouters ?? makeModelRouterStore(getDb());
  const connectors = deps?.connectors ?? { list: () => makeConnectorStore(getDb()).list() };
  const integrationEvents = () => deps?.integrationEvents ?? makeIntegrationEventStore(getDb());
  const connections = () => deps?.connections ?? makeIntegrationConnectionStore(getDb());
  const eventSample = deps?.eventSample ?? loadEventSample;
  const aliasesFor = deps?.aliases ?? makeWebhookAliasResolver();
  const starter = () => deps?.workflowStarter ?? defaultWorkflowStarter();
  const sender = () => deps?.sender ?? defaultAutomationSender;
  const inputKeyOptions =
    deps?.inputKeyOptions ?? productionInputKeyOptions(integrationEvents, connections);
  const now = deps?.now ?? (() => new Date());
  const randomId = deps?.randomId ?? (() => crypto.randomUUID());
  const evalCode = deps?.evalCode ?? evaluateCode;
  const rateLimiter = makeEvalRateLimiter();
  const randomSecret =
    deps?.randomSecret ??
    (() => Buffer.from(crypto.getRandomValues(new Uint8Array(32))).toString("base64url"));
  const orgSecret: OrgSecretClient =
    deps?.orgSecret ?? {
      putSecret: (req) => defaultOrgSecret.putSecret(req),
      deleteSecret: (req) => defaultOrgSecret.deleteSecret(req),
    };

  async function connectorWithWebhookFacet(
    provider: string,
  ): Promise<{ connector: Connector; webhook: NonNullable<Connector["webhook"]> }> {
    const connector = (await loadRegistry(connectors)).get(provider);
    const webhook = connector?.webhook;
    if (!connector || !webhook) {
      throw new BlockValidationError([
        blockError("", "trigger.provider", "unknown_provider", `provider "${provider}" has no webhook event catalog`),
      ]);
    }
    return { connector, webhook };
  }

  /** Parse + validate a definition JSON, with trigger-level checks that need
   * the registry/store (cron schedule, registration existence, declared
   * event keys). */
  async function parseDefinition(
    raw: string,
    kind: "user" | "builtin",
  ): Promise<{ definition: AutomationDefinition; nextFireAt: Date | null }> {
    const parsed = parseJson(raw, "definition_json", DEFINITION_MAX_CHARS);
    let definition: AutomationDefinition;
    try {
      definition = validateDefinition(parsed, { kind });
    } catch (error) {
      const be = toBlockError(error);
      if (be) throw new BlockValidationError([be]);
      throw error;
    }
    await validateSessionBlocks(definition);
    let nextFireAt: Date | null = null;
    const trigger = definition.trigger;
    if (trigger.kind === "cron") {
      nextFireAt = nextCronFire(trigger.schedule, trigger.timezone, now());
    } else if (trigger.kind === "webhook") {
      if (!(await store.getRegistration(trigger.registrationId))) {
        throw new BlockValidationError([
          blockError("", "trigger.registrationId", "unknown_registration", "webhook registration not found"),
        ]);
      }
    } else if (trigger.kind === "integration") {
      const { webhook } = await connectorWithWebhookFacet(trigger.provider);
      const declared = new Set(webhook.events.map((event) => event.key));
      const unknown = trigger.eventKeys.filter((key) => !declared.has(key));
      if (unknown.length > 0) {
        throw new BlockValidationError([
          blockError(
            "",
            "trigger.eventKeys",
            "unknown_event",
            `provider "${trigger.provider}" does not declare event keys: ${unknown.join(", ")}`,
          ),
        ]);
      }
    }
    return { definition, nextFireAt };
  }

  /**
   * ADR 0063 B2, per create_session block: the profile must be active, and a
   * model/effort override is only meaningful next to one harness, so the
   * EFFECTIVE harness is validated against the live catalog and PINNED onto
   * the block config (an admin switching the profile's harness later cannot
   * orphan a stored model id). Blocks with no override never read the
   * catalog.
   */
  async function validateSessionBlocks(definition: AutomationDefinition): Promise<void> {
    const walk = async (blocks: AutomationDefinition["blocks"]): Promise<void> => {
      for (const block of blocks) {
        if (block.type === "create_session") {
          const config = block.config as {
            profileId?: unknown;
            harness?: unknown;
            model?: unknown;
            modelRouter?: unknown;
            effort?: unknown;
          };
          const profileId = typeof config.profileId === "string" ? config.profileId : "";
          const profile = await profiles().getActive(profileId);
          if (!profile) {
            throw new BlockValidationError([
              blockError(block.id, "profileId", "unknown_profile", "profileId is not an active profile"),
            ]);
          }
          const hasOverride =
            config.harness !== undefined ||
            config.model !== undefined ||
            config.modelRouter !== undefined ||
            config.effort !== undefined;
          if (hasOverride) {
            const harness = typeof config.harness === "string" ? config.harness : profile.harness;
            const { harnesses } = await harnessCatalog().listHarnesses({});
            const descriptor = harnesses.find((h) => h.name === harness)?.descriptor;
            if (!descriptor) {
              throw new BlockValidationError([
                blockError(block.id, "harness", "unknown_harness", `harness "${harness}" is not in the catalog`),
              ]);
            }
            const model = typeof config.model === "string" ? config.model : undefined;
            const routerId =
              typeof config.modelRouter === "string" && config.modelRouter !== "" ? config.modelRouter : undefined;
            const effort = typeof config.effort === "string" ? config.effort : undefined;
            if (routerId !== undefined) {
              const router = getModelRouterDefinition(routerId);
              if (!router) {
                throw new BlockValidationError([
                  blockError(block.id, "modelRouter", "unknown_router", `model router "${routerId}" is not registered`),
                ]);
              }
              if (!selectRouterProtocol(router, descriptor.routerProtocols ?? [])) {
                throw new BlockValidationError([
                  blockError(
                    block.id,
                    "modelRouter",
                    "unsupported_router",
                    `harness "${harness}" does not support model router "${router.id}"`,
                  ),
                ]);
              }
              if (model !== undefined && !(await modelRouters().getModel(router.id, model))) {
                throw new BlockValidationError([
                  blockError(block.id, "model", "unknown_model", `model "${model}" is not in router "${router.id}"`),
                ]);
              }
            } else if (model !== undefined && !(descriptor.models ?? []).some((m) => m.id === model)) {
              throw new BlockValidationError([
                blockError(block.id, "model", "unknown_model", `model "${model}" is not valid for harness "${harness}"`),
              ]);
            }
            if (effort !== undefined && !(descriptor.effort ?? []).some((e) => e.id === effort)) {
              throw new BlockValidationError([
                blockError(block.id, "effort", "unknown_effort", `effort "${effort}" is not valid for harness "${harness}"`),
              ]);
            }
            // Pin the effective harness so "follow the profile" can never
            // silently mean "keep a foreign model id".
            block.config = { ...block.config, harness };
          }
        }
        if (block.then) await walk(block.then);
        if (block.else) await walk(block.else);
        if (block.body) await walk(block.body);
      }
    };
    await walk(definition.blocks);
  }

  async function requireAutomation(id: string): Promise<AutomationRow> {
    const row = await store.get(requiredText(id, "automation_id"));
    if (!row) throw new ConnectError("automation not found", Code.NotFound);
    return row;
  }

  function refuseOnBuiltin(row: AutomationRow, what: string): void {
    if (row.kind === "builtin") {
      throw new ConnectError(
        `${what} is not allowed on a built-in automation; edit its inputs or tunable block fields, or duplicate it`,
        Code.PermissionDenied,
      );
    }
  }

  async function summaries(rows: AutomationRow[]) {
    const ids = rows.map((row) => row.id);
    const [latest, counts] = await Promise.all([store.latestRuns(ids), store.runCounts7d(ids, now())]);
    return rows.map((row) => {
      const last = latest.get(row.id);
      return {
        automation: toProtoAutomation(row),
        triggerSummary: triggerSummary(definitionOf(row.version)),
        ...(last ? { lastRun: toProtoRunBrief(last) } : {}),
        runs7d: (counts.get(row.id) ?? []).map(toProtoDayCount),
      };
    });
  }

  /** Resolve the sample a TestRender/DryRun/RunNow should use. */
  async function resolveSample(
    row: AutomationRow,
    sample: { case: "sampleId"; value: string } | { case: "payloadJson"; value: string } | { case: undefined },
  ): Promise<{ payload: Record<string, unknown>; eventKey?: string; receivedAt: Date; aliases: WebhookAliasMapping[] }> {
    const trigger = row.version.trigger;
    let payload: Record<string, unknown> = {};
    let eventKey: string | undefined;
    let receivedAt = now();
    if (sample.case === "payloadJson") {
      payload = parseObjectJson(sample.value, "payload_json", DEFINITION_MAX_CHARS);
    } else if (sample.case === "sampleId") {
      if (trigger.kind === "integration") {
        const event = await integrationEvents().getById(sample.value);
        if (!event) throw new ConnectError("sample not found", Code.NotFound);
        payload = event.payload;
        eventKey = event.eventKey;
        receivedAt = event.receivedAt;
      } else {
        const stored = await store.getSample(sample.value);
        if (!stored) throw new ConnectError("sample not found", Code.NotFound);
        payload = stored.payload;
        eventKey = stored.eventKey;
        receivedAt = stored.receivedAt;
      }
    } else if (trigger.kind === "integration") {
      // Newest ledgered delivery for the first declared key, else the fixture.
      const [first] = trigger.eventKeys;
      const latest = first ? await integrationEvents().getLatest(trigger.connectionId, first) : null;
      if (latest) {
        payload = latest.payload;
        eventKey = latest.eventKey;
        receivedAt = latest.receivedAt;
      } else if (first) {
        payload = eventSample(trigger.provider, first) ?? {};
        eventKey = first;
      }
    } else if (trigger.kind === "webhook") {
      const latest = await store.getLatestSample(trigger.registrationId);
      if (latest) {
        payload = latest.payload;
        eventKey = latest.eventKey;
        receivedAt = latest.receivedAt;
      }
    }
    if (eventKey === undefined && trigger.kind === "integration") eventKey = trigger.eventKeys[0];
    if (eventKey === undefined && trigger.kind === "webhook") eventKey = trigger.events[0];

    let aliases: WebhookAliasMapping[] = [];
    if (trigger.kind === "webhook") {
      aliases = [...(await aliasesFor(trigger.registrationId))];
    } else if (trigger.kind === "integration") {
      aliases = (await loadRegistry(connectors)).get(trigger.provider)?.webhook?.aliases ?? [];
    }
    return { payload, ...(eventKey !== undefined ? { eventKey } : {}), receivedAt, aliases };
  }

  /** Start a run for an automation outside its trigger path (RunNow, DryRun,
   * RetryRun). Goes through the same admission as dispatch. */
  async function startAdHocRun(
    row: AutomationRow,
    input: {
      source: "manual";
      payload: Record<string, unknown>;
      eventKey?: string;
      dryRun: boolean;
      deliveryKey: string;
    },
  ): Promise<string> {
    const definition = effectiveDefinition(row.version, row.blockOverrides);
    const runId = automationRunId(row.id, input.deliveryKey);
    const receivedAt = now().toISOString();
    const outcome = await admitAutomationRun(
      {
        target: { automation: row, definition },
        runId,
        deliveryKey: input.deliveryKey,
        trigger: {
          source: input.source,
          receivedAt,
          payload: input.payload,
          ...(input.eventKey !== undefined ? { eventKey: input.eventKey } : {}),
        },
        scheduledFor: null,
        ...(input.dryRun ? { dryRun: true } : {}),
      },
      { store, starter: starter(), sender: sender(), now },
    );
    if (outcome === "joined") {
      throw new ConnectError(
        "an active run holds this automation's concurrency key; the payload was joined to it",
        Code.FailedPrecondition,
      );
    }
    return runId;
  }

  router.service(AutomationService, {
    async listAutomations(req, ctx) {
      await requireAdmin(ctx, getSession);
      const rows = await store.list({ includeArchived: req.includeArchived });
      // Built-ins pin to the top.
      rows.sort((a, b) => {
        if ((a.kind === "builtin") !== (b.kind === "builtin")) return a.kind === "builtin" ? -1 : 1;
        return a.name.localeCompare(b.name);
      });
      return { automations: await summaries(rows) };
    },

    async getAutomation(req, ctx) {
      await requireAdmin(ctx, getSession);
      let row: AutomationRow | null;
      if (req.lookup.case === "builtinKey") {
        row = await store.getByBuiltinKey(requiredText(req.lookup.value, "builtin_key"));
      } else if (req.lookup.case === "id") {
        row = await store.get(requiredText(req.lookup.value, "id"));
      } else {
        throw new ConnectError("id or builtin_key is required", Code.InvalidArgument);
      }
      if (!row) throw new ConnectError("automation not found", Code.NotFound);
      return { automation: toProtoAutomation(row) };
    },

    async createAutomation(req, ctx) {
      const userId = (await requireAdmin(ctx, getSession)).id;
      const name = requiredText(req.name, "name");
      const { definition, nextFireAt } = await parseDefinition(req.definitionJson, "user");
      const inputs = req.inputsJson ? parseObjectJson(req.inputsJson, "inputs_json") : {};
      assertInputValues(definition.inputsSchema, inputs);
      const row = await store.create(
        {
          name,
          description: req.description.trim(),
          enabled: req.enabled,
          definition,
          nextFireAt: req.enabled ? nextFireAt : null,
          inputs,
        },
        userId,
      );
      return { automation: toProtoAutomation(row) };
    },

    async saveVersion(req, ctx) {
      const userId = (await requireAdmin(ctx, getSession)).id;
      const row = await requireAutomation(req.automationId);
      refuseOnBuiltin(row, "saving a new version");
      if (row.archivedAt) throw new ConnectError("automation is archived", Code.FailedPrecondition);
      const { definition, nextFireAt } = await parseDefinition(req.definitionJson, "user");
      // Existing overrides must still be valid against the new graph; drop
      // the ones that no longer apply rather than refusing the save.
      let overrides: BlockOverrides = row.blockOverrides;
      try {
        applyBlockOverrides(definition, overrides);
      } catch {
        overrides = {};
      }
      const saved = await store.saveVersion(row.id, definition, userId, {
        nextFireAt: row.enabled ? nextFireAt : null,
      });
      if (!saved) throw new ConnectError("automation not found", Code.NotFound);
      if (overrides !== row.blockOverrides) await store.setBlockOverrides(row.id, overrides);
      return { automation: toProtoAutomation((await store.get(row.id)) ?? saved) };
    },

    async listVersions(req, ctx) {
      await requireAdmin(ctx, getSession);
      const row = await requireAutomation(req.automationId);
      return { versions: (await store.listVersions(row.id)).map(toProtoVersion) };
    },

    async setAutomationEnabled(req, ctx) {
      await requireAdmin(ctx, getSession);
      const id = requiredText(req.id, "id");
      const existing = await store.getActive(id);
      if (!existing) throw new ConnectError("automation not found", Code.NotFound);
      const trigger = existing.version.trigger;
      const nextFireAt =
        req.enabled && trigger.kind === "cron"
          ? nextCronFire(trigger.schedule, trigger.timezone, now())
          : req.enabled
            ? undefined
            : null;
      const row = await store.setEnabled(id, req.enabled, nextFireAt);
      if (!row) throw new ConnectError("automation not found", Code.NotFound);
      return { automation: toProtoAutomation(row) };
    },

    async updateAutomationMeta(req, ctx) {
      const userId = (await requireAdmin(ctx, getSession)).id;
      const row = await requireAutomation(req.id);
      if (row.archivedAt) throw new ConnectError("automation is archived", Code.FailedPrecondition);
      const patch = {
        ...(req.name !== undefined ? { name: requiredText(req.name, "name") } : {}),
        ...(req.description !== undefined ? { description: req.description.trim() } : {}),
      };
      if (req.settingsJson !== undefined) {
        refuseOnBuiltin(row, "changing run settings");
        const parsed = settingsSchema.safeParse(parseJson(req.settingsJson, "settings_json", INPUTS_MAX_CHARS));
        if (!parsed.success) {
          const issue = parsed.error.issues[0];
          throw new BlockValidationError([
            blockError("", `settings.${issue?.path.join(".") ?? ""}`, "invalid_settings", issue?.message ?? "invalid settings"),
          ]);
        }
        const settings: AutomationSettings = parsed.data;
        const definition: AutomationDefinition = { ...definitionOf(row.version), settings };
        // Re-validate the whole definition (templates inside settings).
        const { definition: validated } = await parseDefinition(JSON.stringify(definition), "user");
        const saved = await store.saveVersion(row.id, validated, userId, patch);
        if (!saved) throw new ConnectError("automation not found", Code.NotFound);
        return { automation: toProtoAutomation(saved) };
      }
      const updated = await store.updateMeta(row.id, patch);
      if (!updated) throw new ConnectError("automation not found", Code.NotFound);
      return { automation: toProtoAutomation(updated) };
    },

    async setInputs(req, ctx) {
      await requireAdmin(ctx, getSession);
      const row = await requireAutomation(req.automationId);
      const inputs = parseObjectJson(req.inputsJson, "inputs_json");
      // Keys must be declared by the version's inputs schema.
      const declared = new Set(row.version.inputsSchema.map((f) => f.key));
      const unknown = Object.keys(inputs).filter((k) => !declared.has(k));
      if (unknown.length > 0) {
        throw new BlockValidationError(
          unknown.map((k) => blockError("", `inputs.${k}`, "unknown_input", `input "${k}" is not declared`)),
        );
      }
      assertInputValues(row.version.inputsSchema, inputs);
      const updated = await store.setInputs(row.id, inputs);
      if (!updated) throw new ConnectError("automation not found", Code.NotFound);
      return { automation: toProtoAutomation(updated) };
    },

    async setBlockOverrides(req, ctx) {
      await requireAdmin(ctx, getSession);
      const row = await requireAutomation(req.automationId);
      const raw = parseObjectJson(req.overridesJson, "overrides_json");
      const overrides: BlockOverrides = {};
      for (const [blockId, fields] of Object.entries(raw)) {
        if (typeof fields !== "object" || fields === null || Array.isArray(fields)) {
          throw new BlockValidationError([
            blockError(blockId, "", "invalid_override", "override must be an object of field values"),
          ]);
        }
        overrides[blockId] = fields as Record<string, unknown>;
      }
      try {
        applyBlockOverrides(definitionOf(row.version), overrides);
      } catch (error) {
        const be = toBlockError(error);
        if (be) throw new BlockValidationError([be]);
        throw error;
      }
      const updated = await store.setBlockOverrides(row.id, overrides);
      if (!updated) throw new ConnectError("automation not found", Code.NotFound);
      return { automation: toProtoAutomation(updated) };
    },

    async duplicateAutomation(req, ctx) {
      const userId = (await requireAdmin(ctx, getSession)).id;
      const row = await requireAutomation(req.automationId);
      // The copy folds overrides into the graph and is fully editable.
      const definition = effectiveDefinition(row.version, row.blockOverrides);
      const name = req.name?.trim() || `${row.name} (copy)`;
      const nextFireAt =
        definition.trigger.kind === "cron"
          ? nextCronFire(definition.trigger.schedule, definition.trigger.timezone, now())
          : null;
      const copy = await store.create(
        { name, description: row.description, enabled: false, definition, nextFireAt: null, inputs: row.inputs },
        userId,
      );
      void nextFireAt;
      return { automation: toProtoAutomation(copy) };
    },

    async archiveAutomation(req, ctx) {
      await requireAdmin(ctx, getSession);
      const row = await requireAutomation(req.id);
      refuseOnBuiltin(row, "archiving");
      const archived = await store.archive(row.id);
      if (!archived) throw new ConnectError("automation not found", Code.NotFound);
      return { automation: toProtoAutomation(archived) };
    },

    async testRender(req, ctx) {
      await requireAdmin(ctx, getSession);
      const row = await requireAutomation(req.automationId);
      let definition: AutomationDefinition;
      if (req.draftDefinitionJson !== undefined) {
        const parsed = parseJson(req.draftDefinitionJson, "draft_definition_json", DEFINITION_MAX_CHARS);
        try {
          definition = validateDefinition(parsed, { kind: row.kind === "builtin" ? "builtin" : "user" });
        } catch (error) {
          const be = toBlockError(error);
          if (be) return { blocks: [], errors: [be] };
          throw error;
        }
      } else {
        definition = effectiveDefinition(row.version, row.blockOverrides);
      }
      const inputValues =
        req.inputsJson !== undefined ? parseObjectJson(req.inputsJson, "inputs_json") : row.inputs;
      const sample = await resolveSample(row, req.sample);
      const result = await previewDefinition({
        definition,
        inputs: resolveAutomationInputs(definition.inputsSchema, inputValues),
        automationId: row.id,
        automationName: row.name,
        trigger: {
          kind: definition.trigger.kind,
          receivedAt: sample.receivedAt.toISOString(),
          ...(sample.eventKey !== undefined ? { eventKey: sample.eventKey } : {}),
          ...(req.scheduledFor !== undefined ? { scheduledFor: req.scheduledFor } : {}),
          payload: sample.payload,
        },
        aliases: sample.aliases,
      });
      return {
        blocks: result.blocks.map((b) => ({
          blockId: b.blockId,
          blockType: b.blockType,
          renderedJson: JSON.stringify(b.rendered),
          ...(b.filterPass !== undefined ? { filterPass: b.filterPass } : {}),
          scopeJson: JSON.stringify(b.scope),
        })),
        errors: result.errors.map((e) => blockError(e.blockId, e.field, e.code, e.message)),
      };
    },

    async dryRun(req, ctx) {
      await requireAdmin(ctx, getSession);
      const row = await requireAutomation(req.automationId);
      if (row.archivedAt) throw new ConnectError("automation is archived", Code.FailedPrecondition);
      const sample = await resolveSample(row, req.sample);
      const runId = await startAdHocRun(row, {
        source: "manual",
        payload: sample.payload,
        ...(sample.eventKey !== undefined ? { eventKey: sample.eventKey } : {}),
        dryRun: true,
        deliveryKey: `dryrun:${randomId()}`,
      });
      return { runId };
    },

    async runNow(req, ctx) {
      await requireAdmin(ctx, getSession);
      const row = await requireAutomation(req.automationId);
      if (row.archivedAt) throw new ConnectError("automation is archived", Code.FailedPrecondition);
      if (req.inputsJson !== undefined) {
        // A one-off override is applied by persisting it: runs read inputs
        // from the row at snapshot time. Honest and simple; the editor shows
        // the stored values, so "one-off" means "until you change it back".
        const inputs = parseObjectJson(req.inputsJson, "inputs_json");
        const declared = new Set(row.version.inputsSchema.map((f) => f.key));
        const unknown = Object.keys(inputs).filter((k) => !declared.has(k));
        if (unknown.length > 0) {
          throw new BlockValidationError(
            unknown.map((k) => blockError("", `inputs.${k}`, "unknown_input", `input "${k}" is not declared`)),
          );
        }
        const merged = { ...row.inputs, ...inputs };
        assertInputValues(row.version.inputsSchema, merged);
        await store.setInputs(row.id, merged);
      }
      const payload =
        req.payloadJson !== undefined ? parseObjectJson(req.payloadJson, "payload_json", DEFINITION_MAX_CHARS) : {};
      const refreshed = (await store.get(row.id)) ?? row;
      const sample =
        req.payloadJson !== undefined
          ? { payload, eventKey: refreshed.version.trigger.kind === "integration" ? refreshed.version.trigger.eventKeys[0] : undefined }
          : await resolveSample(refreshed, { case: undefined });
      const runId = await startAdHocRun(refreshed, {
        source: "manual",
        payload: sample.payload,
        ...(sample.eventKey !== undefined ? { eventKey: sample.eventKey } : {}),
        dryRun: false,
        deliveryKey: `manual:${randomId()}`,
      });
      return { runId };
    },

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
      let input: CodeInput = {};
      if (req.inputJson !== "") {
        const record = parseObjectJson(req.inputJson, "input_json", EVAL_CODE_INPUT_MAX_CHARS);
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

    async listEventSamples(req, ctx) {
      await requireAdmin(ctx, getSession);
      const row = await requireAutomation(req.automationId);
      const trigger = row.version.trigger;
      const limit = listLimit(req.limit);
      if (trigger.kind === "integration") {
        const rows = await integrationEvents().list(trigger.connectionId, req.eventKey || undefined, limit);
        return {
          samples: rows
            .filter((r) => trigger.eventKeys.includes(r.eventKey))
            .map((r) => ({
              id: r.id,
              eventKey: r.eventKey,
              payloadJson: JSON.stringify(r.payload),
              receivedAt: r.receivedAt.toISOString(),
            })),
        };
      }
      if (trigger.kind === "webhook") {
        const rows = await store.listSamples(trigger.registrationId, req.eventKey || undefined, limit);
        return {
          samples: rows.map((r) => ({
            id: r.id,
            eventKey: r.eventKey,
            payloadJson: JSON.stringify(r.payload),
            receivedAt: r.receivedAt.toISOString(),
          })),
        };
      }
      return { samples: [] };
    },

    async listInputKeyOptions(req, ctx) {
      await requireAdmin(ctx, getSession);
      const noun = requiredText(req.noun, "noun");
      const options = await inputKeyOptions.list(noun, req.connectionId?.trim() || undefined);
      return { options };
    },

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

  router.service(AutomationRunService, {
    async listRuns(req, ctx) {
      await requireAdmin(ctx, getSession);
      const row = await requireAutomation(req.automationId);
      const rows = await store.listRuns(row.id, listLimit(req.limit));
      if (req.includeFiltered) {
        return { runs: rows.map(toProtoRunBrief), filtered: [] };
      }
      // Collapse consecutive filtered runs (newest first) into windows keyed
      // by the real run they precede.
      const runs: ProtoRunBrief[] = [];
      const filtered: Array<{ count: number; firstAt: string; lastAt: string; beforeRunId: string }> = [];
      let window: { count: number; firstAt: Date; lastAt: Date } | null = null;
      const flush = (beforeRunId: string) => {
        if (!window) return;
        filtered.push({
          count: window.count,
          firstAt: window.firstAt.toISOString(),
          lastAt: window.lastAt.toISOString(),
          beforeRunId,
        });
        window = null;
      };
      for (const run of rows) {
        if (run.status === "filtered") {
          if (!window) window = { count: 0, firstAt: run.createdAt, lastAt: run.createdAt };
          window.count += 1;
          if (run.createdAt < window.firstAt) window.firstAt = run.createdAt;
          if (run.createdAt > window.lastAt) window.lastAt = run.createdAt;
          continue;
        }
        flush(run.id);
        runs.push(toProtoRunBrief(run));
      }
      flush("");
      return { runs, filtered };
    },

    async getRun(req, ctx) {
      await requireAdmin(ctx, getSession);
      const run = await store.getRun(requiredText(req.runId, "run_id"));
      if (!run) throw new ConnectError("run not found", Code.NotFound);
      const [steps, sessions] = await Promise.all([
        store.listStepRuns(run.id),
        store.listRunSessionIds(run.id),
      ]);
      const sessionByBlock = new Map(sessions.map((s) => [s.blockId, s.sessionId]));
      return {
        run: {
          brief: toProtoRunBrief(run),
          triggerJson: JSON.stringify(run.trigger),
          steps: steps.map((s) => toProtoStepRun(s, sessionByBlock.get(s.blockId.replace(/\[\d+\]$/, "")))),
          sessionIds: sessions.map((s) => s.sessionId),
        },
      };
    },

    async stopRun(req, ctx) {
      await requireAdmin(ctx, getSession);
      const run = await store.getRun(requiredText(req.runId, "run_id"));
      if (!run) throw new ConnectError("run not found", Code.NotFound);
      if (run.status !== "running" && run.status !== "waiting" && run.status !== "pending") {
        return { sent: false };
      }
      const requestId = randomId();
      await sender().send(
        run.id,
        { kind: "stop", ...(req.reason ? { reason: req.reason } : {}) },
        inboxKeys.stop(run.id, requestId),
      );
      return { sent: true };
    },

    async retryRun(req, ctx) {
      await requireAdmin(ctx, getSession);
      const run = await store.getRun(requiredText(req.runId, "run_id"));
      if (!run) throw new ConnectError("run not found", Code.NotFound);
      const row = await requireAutomation(run.automationId);
      if (row.archivedAt) throw new ConnectError("automation is archived", Code.FailedPrecondition);
      // v1: always from the start with the same trigger payload. from_step_id
      // is accepted and recorded for the phase that implements replay-to-step.
      const runId = await startAdHocRun(row, {
        source: "manual",
        payload: run.trigger.payload ?? {},
        ...(run.trigger.eventKey !== undefined ? { eventKey: run.trigger.eventKey } : {}),
        dryRun: run.dryRun,
        deliveryKey: `retry:${randomId()}`,
      });
      return { runId };
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
        if (connector.webhook.ingress) {
          throw new ConnectError(
            `${providerHint} events arrive through the integration ingress; use an integration trigger instead of a custom webhook`,
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
          .filter((event) => event.hidden !== true)
          .map((event) => [
            event.key,
            { key: event.key, displayName: event.label, observed: observed.has(event.key) },
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
