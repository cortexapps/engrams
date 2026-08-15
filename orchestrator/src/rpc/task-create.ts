/**
 * Task creation — profile→session compilation + the create-a-task primitive
 * (ADR 0053/0055/0056/0057; extracted in ADR 0060 P2.7, unified here).
 *
 * `createTaskWithSession` is the ONE path that turns a profile into a running
 * agent: compile the CreateSession request, reserve its ID, persist the `task`
 * + primary `task_session` authorization snapshot, then boot the upstream
 * session (compensating both stores if the boot fails). Both the TaskService
 * CreateTask RPC (UI chat tasks) and the external-trigger ThreadControlPlane
 * (ADR 0060 Slack threads) call it, so a triggered session runs with the SAME
 * capabilities/network/secrets/skills as a UI task (no new privilege path) and
 * a session is NEVER created outside the task model.
 *
 * `compileSessionCreateInput` is the intricate inner step — image resolution,
 * harness env (user token + CLI dummy env + profile env + git attribution +
 * trigger extras), skills union, and the compiled per-session integration
 * policy.
 */

import { ConnectError, Code } from "@connectrpc/connect";
import { and, eq } from "drizzle-orm";
import type { NodePgDatabase } from "drizzle-orm/node-postgres";

import { log as rootLog } from "../log.ts";
import type { ProfileRow, ProfileStore } from "../db/profiles.ts";
import { makeUserIdentityStore, type UserIdentityStore } from "../db/users.ts";
import { isServiceAccountEmail } from "./api-key.ts";
import {
  makeSessionAppStore,
  type SessionAppRow,
  type SessionAppStore,
} from "../db/session-apps.ts";
import { buildIngressEnv, interpolateEnv, normalizeApps } from "../apps/env.ts";
import { config } from "../config.ts";
import type { ImagesClient } from "./profiles.ts";
import { evictOwnerCacheEntry } from "../authz/resolve.ts";
import {
  sessionListener as sessionListenerTable,
  slackSession as slackSessionTable,
  task as taskTable,
  taskSession as taskSessionTable,
  type ProfileNetwork,
  type IntegrationConnectionSnapshot,
} from "../db/schema.ts";
import * as schema from "../db/schema.ts";
import {
  compileIntegrationPolicy,
  compileCliIntegrations,
  policyHasContent,
  loadRegistry,
  type CustomConnectorSource,
  type IntegrationSecretJson,
} from "../connectors/registry.ts";
import { compileToolManifest } from "../tools/manifest.ts";
import { tools as productionTools, type ToolRegistry } from "../tools/registry.ts";
import { systemPromptForTaskType } from "../prompts/base.ts";
import type { SpecPromptContext } from "../prompts/spec-mode.ts";
import { OauthSubjectKind } from "../gen/engram/app/v1/oauth_pb.ts";
import {
  oauthCredential as defaultOAuthCredential,
  orgSecret as defaultOrgSecret,
} from "../control-plane/client.ts";
import {
  makeIntegrationConnectionStore,
  type IntegrationConnectionStore,
} from "../db/integration-connections.ts";
import {
  defaultConnectionGrants,
  grantsToCapabilities,
  integrationSnapshotHash,
  resolveIntegrationGrants,
  withConnectionMemo,
} from "../integrations/grants.ts";
import {
  compileProviderPolicy,
  providerCliSurfaces,
  providerGuestBundles,
  providerGuestEnv,
  providerGuestServices,
} from "../integrations/providers/index.ts";
import { makeModelRouterStore, type ModelRouterStore } from "../db/model-routers.ts";
import {
  getModelRouterDefinition,
  selectRouterProtocol,
} from "../model-routers/registry.ts";

const log = rootLog.child({ component: "task" });

/** Max length (in code points) of the truncated-prompt default title. */
const DEFAULT_TITLE_MAX_CHARS = 80;

/**
 * Derive a session's initial (default) title from the user's prompt: collapse
 * whitespace/newlines to single spaces, trim, and clip to
 * `DEFAULT_TITLE_MAX_CHARS` code points with an ellipsis. Code-point-aware
 * (`[...s]`) so a multi-byte glyph / emoji is never split mid-surrogate.
 * Returns `null` for an empty/whitespace-only prompt (no default).
 */
export function truncatePrompt(prompt: string | undefined | null): string | null {
  if (prompt == null) return null;
  const collapsed = prompt.replace(/\s+/g, " ").trim();
  if (collapsed === "") return null;
  const chars = [...collapsed];
  if (chars.length <= DEFAULT_TITLE_MAX_CHARS) return collapsed;
  return `${chars.slice(0, DEFAULT_TITLE_MAX_CHARS).join("").trimEnd()}…`;
}

/** Drizzle DB handle the task-persist transaction runs on. */
export type Db = NodePgDatabase<typeof schema>;

// ---------------------------------------------------------------------------
// Profile → CreateSession compilation
// ---------------------------------------------------------------------------

/** The control-plane CreateSession request the orchestrator compiles. Mirrors
 *  the `SessionsClient.createSession` request shape (rpc/tasks.ts). */
export interface SessionCreateInput {
  /** ADR 0109: reserved before boot so the authorization snapshot exists. */
  requestedSessionId?: string;
  /** ADR 0118: the session's apps, already resolved to public hostnames. The
   *  coordinator persists them and hands them to the host's egress proxy, which
   *  uses them to splice a same-session app-to-app call back into the sandbox
   *  instead of sending it out to the internet. */
  apps?: Array<{ hostname: string; port: number }>;
  imageUri: string;
  mode: string;
  prompt?: string;
  harnessEnv?: Record<string, string>;
  selectedSkills?: string[];
  capabilities?: string[];
  integrationPolicyJson?: string;
  /** Immutable connection authority persisted before coordinator boot. */
  integrationGrants?: ProfileRow["integrationGrants"];
  /** Immutable connection configuration used by host-side refresh. */
  integrationConnections?: IntegrationConnectionSnapshot[];
  /** ADR 0062/0063: the selected harness (catalog name) the coordinator mounts
   *  + execs (the proto `CreateSessionRequest.harness`). Resolved from the
   *  per-session override ?? profile ?? deployment default. */
  harness?: string;
  /** ADR 0063 B2 echo: the resolved model/effort catalog option ids. They only
   *  ride the task row for display — the coordinator learns them via the
   *  compiled harness env, never via these fields. */
  model?: string;
  modelRouter?: string;
  effort?: string;
  /** ADR 0106: provider + opaque owner only; never contains OAuth bytes. */
  oauthCredential?: {
    subject: { kind: OauthSubjectKind; id: string };
    provider: string;
  };
  /** ADR 0107: session mode for the initial prompt (e.g. "plan"). */
  harnessMode?: string;
}

/** One harness's catalog descriptor (the bits the compiler needs): the model +
 *  effort enums map an option id → the env vars that select it (ADR 0063 §1). */
export interface HarnessDescriptorView {
  /** Human label for error/UI copy; falls back to the catalog name. */
  label?: string;
  /** The env-var names the harness authenticates with (ADR 0063 §1): `userEnv`
   *  is the human credential (per-user token, injected for human tasks);
   *  `orgEnv` is the programmatic credential (B4, host-side resolved). The
   *  `*Hint` fields are free-text setup instructions surfaced to the user
   *  (e.g. "Run `claude setup-token`"). */
  auth?: {
    userEnv?: string;
    userOauth?: { provider: string; delivery: number };
    orgEnv?: string;
    userEnvHint?: string;
    orgEnvHint?: string;
  };
  models: Array<{
    id: string;
    default: boolean;
    env: Record<string, string>;
  }>;
  effort: Array<{
    id: string;
    default: boolean;
    env: Record<string, string>;
  }>;
  /** ADR 0107: declared session modes (pure declaration — no env). */
  modes?: Array<{ id: string; default: boolean }>;
  routerProtocols?: string[];
  egress?: { allowHosts?: string[]; allowHostPatterns?: string[] };
  nativeEgress?: { allowHosts?: string[]; allowHostPatterns?: string[] };
}
export interface HarnessCatalogClient {
  listHarnesses(req: Record<string, never>): Promise<{
    harnesses: Array<{ name: string; descriptor?: HarnessDescriptorView }>;
  }>;
}

/** Name-only slice of OrgSecretService. Secret values never cross this seam. */
export interface OrgSecretNameClient {
  listSecrets(req: Record<string, never>): Promise<{ secrets: Array<{ name: string }> }>;
}

export interface SessionCompileDeps {
  images: ImagesClient;
  connectors: CustomConnectorSource;
  harnessCatalog: HarnessCatalogClient;
  /** Tool registry to compile into the harness manifest. Production uses the
   *  process-wide registry; tests may inject a focused registry. */
  toolRegistry?: ToolRegistry;
  /** Resolve the owner's harness token for `envVar` (e.g. CLAUDE_CODE_OAUTH_TOKEN),
   *  or null. Called for every human run to inject (and gate on) the selected
   *  harness's declared user credential. */
  resolveUserToken: (envVar: string) => Promise<string | null>;
  /** Resolve ALL of the owner's saved harness tokens (envVar → value). Called
   *  only when the profile sets includeUserTokens, to additionally carry the
   *  user's OTHER credentials into the sandbox. */
  resolveAllUserTokens: () => Promise<Record<string, string>>;
  /** Name-only org-secret lookup used to reject unresolved descriptor refs.
   *  Production uses the control-plane client; tests may inject a fake. */
  orgSecret?: OrgSecretNameClient;
  /** Resolve whether the human owner has a live provider connection. */
  hasOAuthCredential?: (provider: string) => Promise<boolean>;
  oauthSubject?: { kind: OauthSubjectKind; id: string };
  /** ADR 0115: list the owner's personal CONNECTOR credentials (subject kind
   *  `user_connector`) — one call serves every user-scoped integration gate.
   *  Only `status === "connected"` rows satisfy the gate (a broken credential
   *  must block, not boot an unauthenticated session). */
  listUserConnectorCredentials?: () => Promise<Array<{ provider: string; status: string }>>;
  connections: IntegrationConnectionStore;
  modelRouters?: ModelRouterStore;
}

export interface SessionCompileOpts {
  prompt?: string;
  /** Orchestrator task context used to select prompt and tool surfaces. This
   *  value is not sent to the sandbox. */
  taskType?: string;
  /** ADR 0114 D6: the template snapshot that the new spec owns. It shapes the
   *  spec-mode system prompt (structure, done criteria, and process stages) and
   *  is used only when `taskType` is "spec". A `SpecTemplateSnapshot` from the
   *  template catalog satisfies this shape. */
  specTemplate?: SpecPromptContext;
  /** ADR 0107: session mode riding the initial prompt (e.g. "plan").
   *  Validated against the selected harness's declared modes. */
  harnessMode?: string;
  /** Per-session integration grants layered on top of the profile. These may
   *  affect the bound capabilities and integration policy, but never the tool
   *  manifest (for example, a scoped clone credential). */
  extraCapabilities?: readonly string[];
  /** Replace every profile/per-session capability with this exact set. */
  capabilityOverride?: readonly string[];
  /** Replace the profile's network policy for this session. */
  networkOverride?: ProfileNetwork;
  /** Exclude profile-defined secrets and harness env from this session. */
  dropProfileSecretsAndEnv?: boolean;
  /** The task type ("chat" = human/interactive; anything else = programmatic,
   *  e.g. "slack_thread"). Drives the strict-by-run-type credential pick (ADR
   *  0063 B4): human → the harness's `user_env` (per-user token); programmatic →
   *  its `org_env` (org secret, resolved host-side). Default "chat". */
  /** The creator is a service-account principal (an ADR 0086 API key — e.g. a
   *  `ci-<repo>` CI key). Picks the PROGRAMMATIC credential (`org_env`): a
   *  service account has no per-user harness token. Human-owned tasks get the
   *  owner's token regardless of surface (chat UI, Slack, …). */
  programmatic?: boolean;
  /** ADR 0063 B2: per-session override of the profile's default harness / model /
   *  effort. Unset = use the profile's default. */
  harness?: string;
  model?: string;
  /** Empty or absent selects the harness's direct/native provider. */
  modelRouter?: string;
  effort?: string;
  /** Extra harness env with the highest configurable precedence — e.g. the
   *  trigger's ENGRAM_APPEND_SYSTEM_PROMPT (ADR 0060). Native auth names are
   *  ignored, and the principal-owned human user token is applied afterward. */
  extraHarnessEnv?: Record<string, string>;
  /** The initiating human's identity for git commit attribution (ADR 0031 §7),
   *  stamped as ENGRAM_USER_NAME/ENGRAM_USER_EMAIL — the guest writes them into
   *  /etc/gitconfig's [user] block so in-session commits are authored by the
   *  human who started the session. Omit for service-account owners. */
  owner?: { name: string; email: string };
  /** Child sessions report blockers through output and cannot pause on a
   *  direct human interaction tool (ADR 0113). */
  excludeHumanInteractionTools?: boolean;
}

/** The deployment's fallback harness when neither the session nor the profile
 *  selects one (the canonical built-in). */
const DEFAULT_HARNESS = "claude";

/**
 * Compile a CreateSession request from an active profile. Throws
 * `FailedPrecondition` if the profile's image is no longer enabled.
 */
export async function compileSessionCreateInput(
  profile: ProfileRow,
  deps: SessionCompileDeps,
  opts: SessionCompileOpts = {},
): Promise<SessionCreateInput> {
  // Resolve image_id → current image_uri (defense in depth behind DisableImage).
  const catalog = await deps.images.listEnabledImages({});
  const image = catalog.images.find((i) => i.id === profile.imageId);
  if (!image) {
    throw new ConnectError(
      "the profile's image is no longer enabled — contact an admin",
      Code.FailedPrecondition,
    );
  }

  // ADR 0062/0063/0117: resolve the effective harness/router/model/effort (per-session
  // override < profile default < deployment/descriptor default).
  const selectedHarness = opts.harness ?? profile.harness ?? DEFAULT_HARNESS;
  const selectedRouterId = opts.modelRouter ?? profile.modelRouter ?? undefined;
  const { harnesses } = await deps.harnessCatalog.listHarnesses({});
  const descriptor = harnesses.find((h) => h.name === selectedHarness)?.descriptor;
  if (!descriptor) {
    throw new ConnectError(`harness \`${selectedHarness}\` is not in the catalog`, Code.FailedPrecondition);
  }
  const router = selectedRouterId ? getModelRouterDefinition(selectedRouterId) : null;
  if (selectedRouterId && !router) {
    throw new ConnectError(`model router \`${selectedRouterId}\` is not registered`, Code.InvalidArgument);
  }
  const routerProtocol = router ? selectRouterProtocol(router, descriptor.routerProtocols ?? []) : null;
  if (router && !routerProtocol) {
    throw new ConnectError(
      `harness \`${selectedHarness}\` has no protocol adapter for model router \`${router.id}\``,
      Code.FailedPrecondition,
    );
  }
  const modelId =
    opts.model ??
    profile.model ??
    router?.defaultModel ??
    descriptor.models.find((m) => m.default)?.id ??
    descriptor.models[0]?.id;
  let effortId: string | undefined =
    opts.effort ??
    profile.effort ??
    descriptor.effort.find((e) => e.default)?.id ??
    descriptor.effort[0]?.id;
  const modelOption = router ? undefined : descriptor.models.find((m) => m.id === modelId);
  let routedModel;
  if (router) {
    if (!modelId) throw new ConnectError("a routed launch requires a model", Code.InvalidArgument);
    routedModel = await (deps.modelRouters ?? makeModelRouterStore()).getModel(router.id, modelId);
    if (!routedModel || !routedModel.available) {
      throw new ConnectError(`router model \`${modelId}\` is unavailable`, Code.FailedPrecondition);
    }
    if (!routedModel.enabled || (isHumanPrincipal(opts) && !routedModel.userEnabled)) {
      log.warn({ routerId: router.id, modelId, human: isHumanPrincipal(opts) }, "model router policy rejected launch");
      throw new ConnectError(`router model \`${modelId}\` is not enabled for this principal`, Code.PermissionDenied);
    }
    if (!routedModel.supportedParameters.includes("reasoning") && !routedModel.supportedParameters.includes("reasoning_effort")) {
      effortId = undefined;
    }
    const client: OrgSecretNameClient = deps.orgSecret ?? defaultOrgSecret;
    const available = new Set((await client.listSecrets({})).secrets.map((secret) => secret.name));
    if (!available.has(router.credentialSecret)) {
      throw new ConnectError(
        `${router.label} needs the organization secret ${router.credentialSecret}`,
        Code.FailedPrecondition,
      );
    }
    log.info(
      {
        routerId: router.id,
        modelId,
        harness: selectedHarness,
        principal: isHumanPrincipal(opts) ? "human" : "programmatic",
      },
      "model router launch accepted",
    );
  } else if (modelId && !modelOption) {
    throw new ConnectError(`model \`${modelId}\` is not valid for harness \`${selectedHarness}\``, Code.InvalidArgument);
  }
  const effortOption = descriptor.effort.find((e) => e.id === effortId);

  // ADR 0107: a create-time session mode must be one the harness declares.
  // The coordinator re-validates; failing fast here gives the create surface
  // a clean error instead of a queued-then-rejected first prompt.
  if (
    opts.harnessMode != null &&
    descriptor != null &&
    !(descriptor.modes ?? []).some((mode) => mode.id === opts.harnessMode)
  ) {
    throw new ConnectError(
      `harness \`${selectedHarness}\` does not declare mode \`${opts.harnessMode}\``,
      Code.InvalidArgument,
    );
  }

  // Strict-by-principal credentials (ADR 0063 B4, amended): a human-owned task
  // carries the owner's per-user token; a service-account-created task carries
  // the org secret. They are mutually exclusive — never both. The PRINCIPAL
  // decides, never the task type/surface: a Slack mention email-matched to a
  // real user is that user (the old `type === "chat"` gate booted Slack
  // sessions credential-less — "Not logged in", session e721311e), while an
  // API-key creator is programmatic even for a "chat" task.
  const isHuman = isHumanPrincipal(opts);

  // General harness env, lowest → highest precedence: other user tokens < CLI
  // dummy env < profile env_vars < strip native auth names < model env < effort
  // env < git attribution < trigger extras < human user token. NEVER log values.
  const harness: Record<string, string> = {};
  // The human credential env-var name is the selected harness's declared
  // `user_env` (ADR 0063 — no longer the hardcoded CLAUDE_CODE_OAUTH_TOKEN).
  const userEnv = descriptor?.auth?.userEnv;
  const userOauth = descriptor?.auth?.userOauth;
  const orgEnv = descriptor?.auth?.orgEnv;
  let humanUserToken: string | undefined;
  let oauthCredential: SessionCreateInput["oauthCredential"];
  if (isHuman && !router) {
    // The declared user credential is MANDATORY for a human run — a
    // session without it boots unauthenticated. Always inject it, and BLOCK
    // the create when the user hasn't set it (surfacing the descriptor's setup
    // hint) rather than silently booting an un-authed session.
    if (userEnv) {
      const userToken = await deps.resolveUserToken(userEnv);
      if (!userToken) {
        const label = descriptor?.label || selectedHarness;
        const hint = descriptor?.auth?.userEnvHint;
        throw new ConnectError(
          `${label} needs your ${userEnv} credential, which isn't set.` +
            (hint ? ` ${hint}` : "") +
            ` Add it under Settings → Credentials, then start the task again.`,
          Code.FailedPrecondition,
        );
      }
      humanUserToken = userToken;
    }
    if (userOauth) {
      const connected = await deps.hasOAuthCredential?.(userOauth.provider);
      if (!connected || !deps.oauthSubject) {
        const label = descriptor?.label || selectedHarness;
        const hint = descriptor?.auth?.userEnvHint;
        throw new ConnectError(
          `${label} needs your ${userOauth.provider} connection.` +
            (hint ? ` ${hint}` : "") +
            ` Connect it under Settings → Credentials, then start the task again.`,
          Code.FailedPrecondition,
        );
      }
      oauthCredential = { subject: deps.oauthSubject, provider: userOauth.provider };
    }
    // The profile toggle additionally carries the user's OTHER saved tokens
    // (credentials for other harnesses / tools) into the sandbox.
    if (profile.includeUserTokens) {
      for (const [k, v] of Object.entries(await deps.resolveAllUserTokens())) harness[k] = v;
    }
  }
  const registry = await loadRegistry(deps.connectors);
  // One memo per create: every grant-resolution step below reuses the rows the
  // first step fetched, so a create resolves each connection id exactly once.
  const connections = withConnectionMemo(deps.connections);
  const overrideGrants = await defaultConnectionGrants(
    opts.capabilityOverride ?? opts.extraCapabilities ?? [],
    connections,
  );
  const effectiveGrants =
    opts.capabilityOverride !== undefined
      ? overrideGrants
      : [...profile.integrationGrants, ...overrideGrants];
  const resolvedEffectiveGrants = await resolveIntegrationGrants(effectiveGrants, connections);
  const disabledConnection = resolvedEffectiveGrants.find(({ connection }) => !connection.enabled);
  if (disabledConnection) {
    throw new ConnectError(
      `integration connection "${disabledConnection.connection.alias}" is disabled`,
      Code.FailedPrecondition,
    );
  }
  // ADR 0115 (amended): a user-scoped grant (profile opted the integration
  // into the launching user's personal credential) needs that credential for
  // a HUMAN run — the PRINCIPAL decides, like the harness gate above. A
  // missing or unhealthy personal credential does NOT block the launch and
  // NEVER falls back to the org credential: the integration is disabled for
  // this session — its grants are dropped before capabilities, the tool/CLI
  // surface, the egress policy, and the snapshot are compiled. The start
  // screen mirrors this with a warning so the drop is never silent.
  // Programmatic sessions compile the org credential regardless, and
  // override grants never carry a scope, so an override session never drops.
  const userScopedGrants = isHuman
    ? resolvedEffectiveGrants.filter(
        ({ grant, connection }) =>
          grant.credentialScope === "user" &&
          registry.get(connection.provider)?.userCredential !== undefined,
      )
    : [];
  let unavailableUserProviders = new Set<string>();
  if (userScopedGrants.length > 0) {
    // Fail closed: an unwired lister reads as "no credentials", so the
    // integrations disable rather than silently borrowing org authority.
    const rows = (await deps.listUserConnectorCredentials?.()) ?? [];
    const connected = new Set(
      rows.filter((row) => row.status === "connected").map((row) => row.provider),
    );
    unavailableUserProviders = new Set(
      userScopedGrants
        .map(({ connection }) => connection.provider)
        .filter((provider) => !connected.has(provider)),
    );
  }
  const grantAvailable = ({ grant, connection }: (typeof resolvedEffectiveGrants)[number]) =>
    !(grant.credentialScope === "user" && unavailableUserProviders.has(connection.provider));
  const activeResolvedGrants = resolvedEffectiveGrants.filter(grantAvailable);
  const capabilities = grantsToCapabilities(activeResolvedGrants);
  // A capability override is the complete session authority and therefore
  // also owns its CLI/tool surface. Without one, preserve the narrower
  // profile-owned surface: extra integration grants do not add model tools.
  // The profile-only resolution is LAZY: under an override it never runs (its
  // result would be unused), and without one the memo makes it query-free
  // (profile grants are a subset of the effective grants resolved above).
  const surfacedCapabilities =
    opts.capabilityOverride !== undefined
      ? capabilities
      : grantsToCapabilities(
          (await resolveIntegrationGrants(profile.integrationGrants, connections)).filter(
            grantAvailable,
          ),
        );
  const cliPlan = compileCliIntegrations(surfacedCapabilities, registry);
  for (const [k, v] of Object.entries(cliPlan.dummyEnv)) harness[k] = v;
  // Connector-backed CLIs, then the ones a named connection makes usable. The
  // second list comes from the provider registry, so a new provider surfaces
  // its CLI without a branch here.
  const enabledCli = [...cliPlan.enabled, ...providerCliSurfaces(activeResolvedGrants)];
  if (enabledCli.length > 0) harness.ENGRAM_CLI_INTEGRATIONS = JSON.stringify(enabledCli);
  const baseToolRegistry = deps.toolRegistry ?? productionTools;
  const manifestRegistry: ToolRegistry = opts.excludeHumanInteractionTools
    ? {
        register: (definition) => baseToolRegistry.register(definition),
        get: (name) => baseToolRegistry.get(name),
        all: () =>
          baseToolRegistry
            .all()
            .filter((tool) => tool.name !== "ask_user_question" && tool.name !== "exit_plan_mode"),
        complete: (sessionId, toolCallId, result) =>
          baseToolRegistry.complete(sessionId, toolCallId, result),
      }
    : baseToolRegistry;
  const toolManifest = compileToolManifest(
    manifestRegistry,
    surfacedCapabilities,
    opts.taskType,
  );
  if (toolManifest.length > 0) harness.ENGRAM_TOOLS = JSON.stringify(toolManifest);
  if (!opts.dropProfileSecretsAndEnv) {
    for (const [k, v] of Object.entries(profile.envVars)) harness[k] = v;
  }
  // Native credential names are principal-authoritative, so erase anything
  // carried by user tokens, CLI dummy env, or profile env before descriptor
  // options run. A model may deliberately restore org_env as "" to disable the
  // native credential, but a descriptor may never supply it.
  if (userEnv) delete harness[userEnv];
  if (orgEnv) delete harness[orgEnv];
  // ADR 0063: the selected model/effort map to env vars via the harness
  // descriptor (an explicit picker wins over a stale ANTHROPIC_MODEL in
  // env_vars). Option env values are always plain literals; secret delivery is
  // carried separately in the integration policy.
  for (const [k, v] of Object.entries(modelOption?.env ?? {})) harness[k] = v;
  for (const [k, v] of Object.entries(effortOption?.env ?? {})) harness[k] = v;
  if (router && routerProtocol && modelId) {
    harness.ENGRAM_MODEL_ROUTER_ID = router.id;
    harness.ENGRAM_MODEL_ROUTER_PROTOCOL = routerProtocol;
    harness.ENGRAM_MODEL_ROUTER_BASE_URL = router.protocols[routerProtocol].baseUrl;
    harness.ENGRAM_MODEL_ROUTER_MODEL = modelId;
  }
  // ADR 0031 §7: git commit attribution — the initiating human authors the
  // in-session commits (the guest turns these into /etc/gitconfig's [user]
  // block). Orchestrator-authoritative, so it beats profile env_vars.
  if (opts.owner) {
    harness.ENGRAM_USER_NAME = opts.owner.name;
    harness.ENGRAM_USER_EMAIL = opts.owner.email;
  }
  for (const [k, v] of Object.entries(opts.extraHarnessEnv ?? {})) {
    if (k !== userEnv && k !== orgEnv) harness[k] = v;
  }
  harness.ENGRAM_APPEND_SYSTEM_PROMPT = [
    harness.ENGRAM_APPEND_SYSTEM_PROMPT,
    systemPromptForTaskType(opts.taskType, opts.specTemplate),
  ]
    .filter(Boolean)
    .join("\n\n");
  // ADR 0097: the browser bundle carries a local image-observation tool. It
  // is harness-native (not a connector capability) and is enabled only when
  // the corresponding skill is mounted into this session.
  // Where a guest looks for each present provider's credential. The values
  // come from the provider itself, so a new one needs no branch here.
  Object.assign(harness, providerGuestEnv(activeResolvedGrants));
  const selectedSkills = [
    ...new Set([
      ...profile.skills,
      ...cliPlan.bundles,
      ...providerGuestBundles(activeResolvedGrants),
    ]),
  ];
  if (selectedSkills.includes("browser")) harness.ENGRAM_BROWSER_VIEW_ENABLED = "1";
  else delete harness.ENGRAM_BROWSER_VIEW_ENABLED;

  // The human credential remains the final, principal-authoritative env write,
  // so no descriptor or trigger can replace it.
  if (isHuman && !router && userEnv && humanUserToken !== undefined) harness[userEnv] = humanUserToken;
  const harnessEnv = Object.keys(harness).length > 0 ? harness : undefined;

  // Per-session integration policy (caps + network + secrets), shipped only
  // when it carries content.
  const policy = compileIntegrationPolicy(
    activeResolvedGrants.map(({ grant, connection }) => ({
      connectionId: connection.id,
      provider: connection.provider,
      operation: grant.operation,
      resourceConstraints: grant.resourceConstraints,
      // ADR 0115: only a human compile stamps the user subject below, so a
      // programmatic session's user-scoped grants fall back to org authority.
      userScoped: grant.credentialScope === "user",
    })),
    registry,
    {
      network: opts.networkOverride ?? profile.network,
      secrets: opts.dropProfileSecretsAndEnv ? [] : profile.secrets,
      ...(isHuman && deps.oauthSubject ? { userSubjectId: deps.oauthSubject.id } : {}),
    },
  );
  const routeEgress = router ? router.egressHosts : (descriptor.nativeEgress?.allowHosts ?? []);
  policy.network.allow_hosts = [...new Set([...policy.network.allow_hosts, ...routeEgress])];
  policy.network.allow_host_patterns = [
    ...new Set([
      ...policy.network.allow_host_patterns,
      ...(router ? [] : (descriptor.nativeEgress?.allowHostPatterns ?? [])),
    ]),
  ];
  if (router) {
    const secret: IntegrationSecretJson = {
      secret_ref: router.credentialSecret,
      env_var: "ENGRAM_MODEL_ROUTER_API_KEY",
      mode: "broker",
      allow_hosts: [...router.egressHosts],
      allow_host_patterns: [],
    };
    policy.secrets.push(secret);
  }
  compileProviderPolicy(policy, activeResolvedGrants);
  policy.guest_services = providerGuestServices(activeResolvedGrants);
  // ADR 0063 B4: a programmatic task (cron / Slack / API) authenticates the
  // harness with the ORG credential, not a per-user token. The org-secret value
  // never leaves the coordinator (ADR 0057), so we can't read it here — instead
  // append a literal secret-inject naming the org secret (named after the env
  // var by convention; admins create an org secret `ANTHROPIC_API_KEY`). It
  // ships in integration_policy_json and is resolved host-side by
  // resolve_policy_secrets. An effective model that explicitly sets org_env to
  // "" deliberately disables this native inject.
  const modelDisablesNativeOrgCredential =
    orgEnv !== undefined &&
    modelOption !== undefined &&
    Object.prototype.hasOwnProperty.call(modelOption.env, orgEnv);
  if (!router && !isHuman && orgEnv && !modelDisablesNativeOrgCredential) {
    policy.secrets.push({
      secret_ref: orgEnv,
      env_var: orgEnv,
      mode: "literal",
      allow_hosts: [],
      allow_host_patterns: [],
    });
  }
  const integrationPolicyJson = policyHasContent(policy) ? JSON.stringify(policy) : undefined;

  return {
    imageUri: image.imageUri,
    mode: "agent",
    harness: selectedHarness,
    ...(router ? { modelRouter: router.id } : {}),
    ...(modelId != null ? { model: modelId } : {}),
    ...(effortId != null ? { effort: effortId } : {}),
    ...(opts.prompt != null ? { prompt: opts.prompt } : {}),
    ...(opts.harnessMode != null ? { harnessMode: opts.harnessMode } : {}),
    ...(harnessEnv != null ? { harnessEnv } : {}),
    ...(oauthCredential != null ? { oauthCredential } : {}),
    ...(selectedSkills.length > 0 ? { selectedSkills } : {}),
    ...(capabilities.length > 0 ? { capabilities } : {}),
    ...(integrationPolicyJson != null ? { integrationPolicyJson } : {}),
    // The snapshot records the authority the session ACTUALLY holds — a
    // dropped user-scoped integration leaves no grant behind.
    integrationGrants: activeResolvedGrants.map(({ grant }) => grant),
    integrationConnections: [
      ...new Map(
        activeResolvedGrants.map(({ connection }) => [
          connection.id,
          {
            id: connection.id,
            alias: connection.alias,
            provider: connection.provider,
            displayName: connection.displayName,
            config: structuredClone(connection.config),
          } satisfies IntegrationConnectionSnapshot,
        ]),
      ).values(),
    ],
  };
}

function isHumanPrincipal(opts: SessionCompileOpts): boolean {
  return !opts.programmatic;
}

// ---------------------------------------------------------------------------
// Create-a-task primitive
// ---------------------------------------------------------------------------

/** The upstream session ops the create-task primitive drives — a structural
 *  subset of the full SessionService client, so callers pass their richer
 *  client unchanged. */
export interface TaskSessionsClient {
  createSession(req: SessionCreateInput): Promise<{ sessionId: string }>;
  deleteSession(req: { sessionId: string }): Promise<unknown>;
}

export interface CreateTaskDeps {
  profiles: ProfileStore;
  images: ImagesClient;
  connectors: CustomConnectorSource;
  harnessCatalog: HarnessCatalogClient;
  sessions: TaskSessionsClient;
  /** Injected IDs keep create ordering deterministic in tests. */
  newTaskId?: () => string;
  newSessionId?: () => string;
  /** The owner's per-user harness token store: `get` resolves one env var (the
   *  selected harness's `user_env`); `getAll` resolves every saved token (the
   *  includeUserTokens carry). */
  secrets: {
    get(userId: string, envVar: string): Promise<string | null>;
    getAll(userId: string): Promise<Record<string, string>>;
  };
  oauth?: {
    listCredentials(req: { subject: { kind: OauthSubjectKind; id: string } }): Promise<{
      credentials: Array<{ provider: string; connected: boolean; status: string }>;
    }>;
  };
  db: Db;
  /** ADR 0118: session-app store, used to reserve `profile.apps` hostnames
   *  inside the pre-create transaction.
   *  Defaults to a Drizzle store over `db` when omitted. */
  sessionApps?: SessionAppStore;
  /** ADR 0118: override the preview base domain (default: config.previewBaseDomain). */
  previewBaseDomain?: string;
  /** ADR 0031 §7: owner identity lookup for git commit attribution.
   *  Defaults to a Drizzle store over `db` when omitted. */
  users?: UserIdentityStore;
  connections?: IntegrationConnectionStore;
  modelRouters?: ModelRouterStore;
  orgSecret?: OrgSecretNameClient;
}

export interface CreateTaskParams {
  /** Task type: "chat" (UI) | "slack_thread" (ADR 0060 trigger) | … */
  type: string;
  /** The engrams user who owns the task (createdByUserId → the CASL subject). */
  ownerUserId: string;
  /** The owner is a service-account principal (API key) — forces the
   *  programmatic (org-credential) compile path; see SessionCompileOpts. */
  ownerIsServiceAccount?: boolean;
  /** The profile to start from; must be active (else NotFound). */
  profileId: string;
  title?: string | null;
  prompt?: string;
  /** Create the session without its initial prompt. The browser uploads files
   *  and then sends the prompt through the normal prompt path (ADR 0113). */
  deferInitialPrompt?: boolean;
  /** ADR 0063 B2: per-session override of the profile's harness / model / effort. */
  harness?: string;
  model?: string;
  modelRouter?: string;
  effort?: string;
  /** ADR 0107: session mode for the initial prompt (e.g. "plan"). */
  harnessMode?: string;
  /** ADR 0114 D3: the template snapshot that the new spec owns for its complete
   *  lifetime. Shapes the spec-mode system prompt, and is used only when `type`
   *  is "spec". Pass the snapshot the spec owns, never the live template row. */
  specTemplate?: SpecPromptContext;
  /** Type-specific trigger ref recorded on the task row (operator-visible). */
  source?: Record<string, unknown>;
  /** Extra harness env merged LAST — e.g. the trigger's
   *  ENGRAM_APPEND_SYSTEM_PROMPT (ADR 0060). */
  extraHarnessEnv?: Record<string, string>;
  /** Slack workflow mailbox to bind before the listener becomes discoverable. */
  slackThreadWorkflowId?: string;
}

export interface CreateSessionForExistingTaskParams {
  taskId: string;
  profileId: string;
  role: string;
  /** Persisted task type, supplied by the owning orchestrator workflow. */
  taskType?: string;
  ownerUserId?: string;
  /** Stable principal stamped into the immutable integration snapshot. */
  integrationPrincipalId?: string;
  prompt?: string;
  /** ADR 0107: session mode for the initial prompt (e.g. "plan"). */
  harnessMode?: string;
  /** ADR 0063 B2: per-session override of the profile's harness / model / effort
   *  (an automation's stored selection). Unset = the profile's default. */
  harness?: string;
  model?: string;
  modelRouter?: string;
  effort?: string;
  extraCapabilities?: readonly string[];
  capabilityOverride?: readonly string[];
  networkOverride?: ProfileNetwork;
  dropProfileSecretsAndEnv?: boolean;
  appendSystemPrompt?: string;
  /** Register the session for terminal/event consumption in the same
   * transaction as its task_session row. */
  registerListener?: boolean;
  /** Optional caller context; the existing task already owns its durable
   *  source metadata, so this path does not insert or update it. */
  source?: Record<string, unknown>;
}

export interface CreateSessionForExistingTaskDeps extends Omit<CreateTaskDeps, "profiles"> {
  profiles: Pick<ProfileStore, "getActive">;
}

export interface CreatedTask {
  taskId: string;
  sessionId: string;
}

/** Make an already-persisted session discoverable by the listener scanner.
 * Callers with consumer-specific bindings must persist those bindings first. */
export async function registerSessionListener(db: Db, sessionId: string): Promise<void> {
  await db.insert(sessionListenerTable).values({ sessionId });
}

/**
 * Create a session and attach it to an already-persisted task. Review phases
 * use this path because their automation-owned `pr_review` task is created
 * before any worker session exists. Callers opt into listener registration
 * when their workflow needs terminal session state.
 */
export async function createSessionForExistingTask(
  deps: CreateSessionForExistingTaskDeps,
  params: CreateSessionForExistingTaskParams,
): Promise<{ sessionId: string }> {
  const profile = await deps.profiles.getActive(params.profileId);
  if (!profile) {
    throw new ConnectError("profile not found or archived", Code.NotFound);
  }
  const integrationPrincipalId = params.ownerUserId ?? params.integrationPrincipalId;

  let owner: { name: string; email: string } | undefined;
  if (params.ownerUserId !== undefined) {
    try {
      const identity = await (deps.users ?? makeUserIdentityStore(deps.db)).getIdentity(
        params.ownerUserId,
      );
      if (identity && !isServiceAccountEmail(identity.email)) owner = identity;
    } catch (err) {
      log.warn(
        { userId: params.ownerUserId, err },
        "task-create: owner identity lookup failed — booting without git attribution",
      );
    }
  }

  const sessionInput = await compileSessionCreateInput(
    profile,
    {
      images: deps.images,
      connectors: deps.connectors,
      harnessCatalog: deps.harnessCatalog,
      resolveUserToken: (envVar) =>
        params.ownerUserId === undefined
          ? Promise.resolve(null)
          : deps.secrets.get(params.ownerUserId, envVar),
      resolveAllUserTokens: () =>
        params.ownerUserId === undefined
          ? Promise.resolve({})
          : deps.secrets.getAll(params.ownerUserId),
      ...(params.ownerUserId === undefined
        ? {}
        : {
            oauthSubject: { kind: OauthSubjectKind.USER, id: params.ownerUserId },
            hasOAuthCredential: async (provider: string) => {
              const response = await (deps.oauth ?? defaultOAuthCredential).listCredentials({
                subject: { kind: OauthSubjectKind.USER, id: params.ownerUserId! },
              });
              return response.credentials.some(
                (credential) => credential.provider === provider && credential.connected,
              );
            },
            listUserConnectorCredentials: async () => {
              const response = await (deps.oauth ?? defaultOAuthCredential).listCredentials({
                subject: { kind: OauthSubjectKind.USER_CONNECTOR, id: params.ownerUserId! },
              });
              return response.credentials.map((credential) => ({
                provider: credential.provider,
                status: credential.status,
              }));
            },
          }),
      connections: deps.connections ?? makeIntegrationConnectionStore(deps.db),
      modelRouters: deps.modelRouters,
      orgSecret: deps.orgSecret,
    },
    {
      // An automation-owned review task has no human token; use the harness's
      // programmatic credential while still creating the session promptless.
      ...(params.ownerUserId === undefined ? { programmatic: true } : {}),
      ...(params.taskType !== undefined ? { taskType: params.taskType } : {}),
      ...(params.prompt != null ? { prompt: params.prompt } : {}),
      ...(params.harnessMode != null ? { harnessMode: params.harnessMode } : {}),
      ...(params.harness != null ? { harness: params.harness } : {}),
      ...(params.model != null ? { model: params.model } : {}),
      ...(params.modelRouter != null ? { modelRouter: params.modelRouter } : {}),
      ...(params.effort != null ? { effort: params.effort } : {}),
      ...(params.extraCapabilities ? { extraCapabilities: params.extraCapabilities } : {}),
      ...(params.capabilityOverride !== undefined
        ? { capabilityOverride: params.capabilityOverride }
        : {}),
      ...(params.networkOverride !== undefined ? { networkOverride: params.networkOverride } : {}),
      ...(params.dropProfileSecretsAndEnv !== undefined
        ? { dropProfileSecretsAndEnv: params.dropProfileSecretsAndEnv }
        : {}),
      ...(params.appendSystemPrompt
        ? { extraHarnessEnv: { ENGRAM_APPEND_SYSTEM_PROMPT: params.appendSystemPrompt } }
        : {}),
      ...(owner ? { owner } : {}),
    },
  );

  // ADR 0109: publish the immutable authorization snapshot before coordinator
  // boot. The host-side token broker can now authorize the first VM request.
  const sessionId = deps.newSessionId?.() ?? crypto.randomUUID();
  sessionInput.requestedSessionId = sessionId;
  await deps.db.transaction(async (tx) => {
    await tx.insert(taskSessionTable).values({
      taskId: params.taskId,
      sessionId,
      role: params.role,
      profileId: profile.id,
      capabilities: sessionInput.capabilities ?? [],
      integrationGrants: sessionInput.integrationGrants ?? [],
      integrationConnections: sessionInput.integrationConnections ?? [],
      integrationSnapshotHash: integrationSnapshotHash({
        profileId: profile.id,
        integrationGrants: sessionInput.integrationGrants ?? [],
        integrationConnections: sessionInput.integrationConnections ?? [],
      }),
      ...(integrationPrincipalId ? { integrationPrincipalId } : {}),
    });
  });

  let createdSessionId: string | undefined;
  try {
    // When prompt is omitted (as it is for the finder), the session boots idle
    // so deterministic bootstrap can finish before SendPrompt wakes it.
    const created = await deps.sessions.createSession(sessionInput);
    createdSessionId = created.sessionId;
    if (created.sessionId !== sessionId) {
      throw new Error("coordinator returned a different reserved session ID");
    }
    if (params.registerListener === true) {
      await deps.db.transaction(async (tx) => {
        await tx.insert(sessionListenerTable).values({ sessionId });
      });
    }
  } catch (err) {
    // Compensation must never mask the original failure: guard every step and
    // log what it could not undo. On an ID mismatch, the session that leaks is
    // the one the coordinator ACTUALLY created, so delete that one.
    try {
      await deps.sessions.deleteSession({ sessionId: createdSessionId ?? sessionId });
    } catch (delErr) {
      log.error(
        { sessionId: createdSessionId ?? sessionId, err: delErr },
        "task-create: failed to delete session after create failure",
      );
    }
    try {
      await deps.db.transaction(async (tx) => {
        await tx
          .delete(taskSessionTable)
          .where(
            and(
              eq(taskSessionTable.taskId, params.taskId),
              eq(taskSessionTable.sessionId, sessionId),
            ),
          );
      });
    } catch (dbErr) {
      log.error(
        { taskId: params.taskId, sessionId, err: dbErr },
        "task-create: failed to remove task_session after create failure — manual cleanup needed",
      );
    }
    throw err;
  }

  evictOwnerCacheEntry(sessionId);
  return { sessionId };
}

/**
 * Create a task and its primary session in one atomic operation. Loads the
 * active profile (NotFound if missing/archived), compiles the CreateSession
 * request, then persists the task and its immutable authorization snapshot
 * before coordinator boot. A failed boot removes both records.
 *
 * This is the SINGLE create path — authorization (who may create) is the
 * caller's concern; this primitive only does the mechanical create + persist.
 */
export async function createTaskWithSession(
  deps: CreateTaskDeps,
  params: CreateTaskParams,
): Promise<CreatedTask> {
  const profile = await deps.profiles.getActive(params.profileId);
  if (!profile) {
    throw new ConnectError("profile not found or archived", Code.NotFound);
  }
  // ADR 0031 §7: resolve the owner's identity for git commit attribution.
  // Service-account owners (API-key creates) are skipped — their synthetic
  // `apikey+…@service.local` email is not a valid commit author (GitHub
  // rejects it on squash-merge). Best-effort: a lookup failure boots the
  // session unattributed rather than failing the create.
  let owner: { name: string; email: string } | undefined;
  try {
    const identity = await (deps.users ?? makeUserIdentityStore(deps.db)).getIdentity(
      params.ownerUserId,
    );
    if (identity && !isServiceAccountEmail(identity.email)) owner = identity;
  } catch (err) {
    log.warn(
      { userId: params.ownerUserId, err },
      "task-create: owner identity lookup failed — booting without git attribution",
    );
  }

  const sessionInput = await compileSessionCreateInput(
    profile,
    {
      images: deps.images,
      connectors: deps.connectors,
      harnessCatalog: deps.harnessCatalog,
      resolveUserToken: (envVar) => deps.secrets.get(params.ownerUserId, envVar),
      resolveAllUserTokens: () => deps.secrets.getAll(params.ownerUserId),
      oauthSubject: { kind: OauthSubjectKind.USER, id: params.ownerUserId },
      hasOAuthCredential: async (provider: string) => {
        const response = await (deps.oauth ?? defaultOAuthCredential).listCredentials({
          subject: { kind: OauthSubjectKind.USER, id: params.ownerUserId },
        });
        return response.credentials.some(
          (credential) => credential.provider === provider && credential.connected,
        );
      },
      listUserConnectorCredentials: async () => {
        const response = await (deps.oauth ?? defaultOAuthCredential).listCredentials({
          subject: { kind: OauthSubjectKind.USER_CONNECTOR, id: params.ownerUserId },
        });
        return response.credentials.map((credential) => ({
          provider: credential.provider,
          status: credential.status,
        }));
      },
      connections: deps.connections ?? makeIntegrationConnectionStore(deps.db),
      modelRouters: deps.modelRouters,
      orgSecret: deps.orgSecret,
    },
    {
      taskType: params.type,
      ...(params.ownerIsServiceAccount ? { programmatic: true } : {}),
      ...(params.prompt != null ? { prompt: params.prompt } : {}),
      ...(params.harness != null ? { harness: params.harness } : {}),
      ...(params.model != null ? { model: params.model } : {}),
      ...(params.modelRouter != null ? { modelRouter: params.modelRouter } : {}),
      ...(params.effort != null ? { effort: params.effort } : {}),
      ...(params.harnessMode != null ? { harnessMode: params.harnessMode } : {}),
      ...(params.specTemplate ? { specTemplate: params.specTemplate } : {}),
      ...(params.extraHarnessEnv ? { extraHarnessEnv: params.extraHarnessEnv } : {}),
      ...(owner ? { owner } : {}),
    },
  );

  if (params.deferInitialPrompt) {
    delete sessionInput.prompt;
    delete sessionInput.harnessMode;
  }

  const taskId = deps.newTaskId?.() ?? crypto.randomUUID();
  const sessionId = deps.newSessionId?.() ?? crypto.randomUUID();
  sessionInput.requestedSessionId = sessionId;
  const launchPolicy: schema.TaskLaunchPolicy = {
    version: 1,
    profileId: profile.id,
    imageUri: sessionInput.imageUri,
    harness: sessionInput.harness ?? profile.harness,
    ...(sessionInput.modelRouter != null ? { modelRouter: sessionInput.modelRouter } : {}),
    ...(sessionInput.model != null ? { model: sessionInput.model } : {}),
    ...(sessionInput.effort != null ? { effort: sessionInput.effort } : {}),
    includeUserTokens: profile.includeUserTokens,
    envVars: { ...profile.envVars },
    skills: [...(sessionInput.selectedSkills ?? [])],
    capabilities: [...(sessionInput.capabilities ?? [])],
    integrationPolicyJson: sessionInput.integrationPolicyJson ?? "",
    integrationGrants: [...(sessionInput.integrationGrants ?? [])],
    integrationConnections: [...(sessionInput.integrationConnections ?? [])],
    network: profile.network,
    secrets: [...profile.secrets],
    repos: [...profile.repos],
    apps: profile.apps.map((a) => ({ ...a })),
  };

  // ADR 0118: the apps this session hosts. A malformed declaration is DROPPED,
  // not fatal — one bad app must never stop a session booting — and logged so
  // the mistake is visible. Save-time validation (rpc/profiles.ts) is where a
  // user is told about it, and it rejects rather than drops.
  const { specs: appSpecs, rejected: rejectedApps } = normalizeApps(profile.apps);
  if (rejectedApps.length > 0) {
    log.warn({ sessionId, rejected: rejectedApps }, "task-create: dropped invalid profile apps");
  }
  let appRows: SessionAppRow[] = [];

  // ADR 0109: the broker must see this snapshot before the VM can make its
  // first credentialed request. Listener registration remains post-boot.
  await deps.db.transaction(async (tx) => {
    await tx.insert(taskTable).values({
      id: taskId,
      type: params.type,
      title: params.title ?? truncatePrompt(params.prompt),
      status: "open",
      createdByUserId: params.ownerUserId,
      source: params.source ?? {},
      // ADR 0063 B2 echo: persist the EFFECTIVE selection (already resolved by
      // compileSessionCreateInput) so reads can show what this task runs with.
      harness: sessionInput.harness ?? null,
      modelRouter: sessionInput.modelRouter ?? null,
      model: sessionInput.model ?? null,
      effort: sessionInput.effort ?? null,
      rootTaskId: taskId,
      launchPolicy,
    });
    await tx.insert(taskSessionTable).values({
      taskId,
      sessionId,
      role: "primary",
      profileId: profile.id,
      capabilities: sessionInput.capabilities ?? [],
      integrationGrants: sessionInput.integrationGrants ?? [],
      integrationConnections: sessionInput.integrationConnections ?? [],
      integrationSnapshotHash: integrationSnapshotHash({
        profileId: profile.id,
        integrationGrants: sessionInput.integrationGrants ?? [],
        integrationConnections: sessionInput.integrationConnections ?? [],
      }),
      integrationPrincipalId: params.ownerUserId,
    });
    if (params.slackThreadWorkflowId !== undefined) {
      await tx.insert(slackSessionTable).values({
        sessionId,
        threadWfId: params.slackThreadWorkflowId,
      });
    }
    // ADR 0118: reserve every app's hostname HERE — one batched insert inside a
    // transaction that already runs, before the session exists. That is what
    // makes the addresses available as env vars below, and it costs no extra
    // round trip (it replaces three serial ones per port, post-create).
    if (appSpecs.length > 0) {
      const store = deps.sessionApps ?? makeSessionAppStore(deps.db);
      appRows = await store.createMany(sessionId, params.ownerUserId, appSpecs, tx);
    }
  });

  // ADR 0118: give every process both forms of every app's address, then let
  // profile.envVars remap them into the names this profile's services read.
  // `harnessEnv` becomes the coordinator's identity_env → session_env → the
  // SpawnHarness frame agentd applies to everything it spawns.
  if (appRows.length > 0) {
    const ingress = buildIngressEnv(appRows, deps.previewBaseDomain ?? config.previewBaseDomain);
    const { env: remapped, unresolved } = interpolateEnv(sessionInput.harnessEnv ?? {}, ingress);
    if (unresolved.length > 0) {
      log.warn(
        { sessionId, unresolved },
        "task-create: env references an app address that does not exist (left verbatim)",
      );
    }
    // Ingress vars first so a profile may deliberately override one by name.
    sessionInput.harnessEnv = { ...ingress, ...remapped };
    // ADR 0118 P3: the coordinator needs the resolved addresses too, so the
    // host's egress proxy can recognise one of this guest's own hostnames at
    // SNI-peek time and splice the call back in.
    sessionInput.apps = appRows.map((r) => ({
      hostname: `${r.hostLabel}.${deps.previewBaseDomain ?? config.previewBaseDomain}`,
      port: r.port,
    }));
  }

  let createdSessionId: string | undefined;
  try {
    const created = await deps.sessions.createSession(sessionInput);
    createdSessionId = created.sessionId;
    if (created.sessionId !== sessionId) {
      throw new Error("coordinator returned a different reserved session ID");
    }
    await deps.db.transaction(async (tx) => {
      await tx.insert(sessionListenerTable).values({ sessionId });
    });
  } catch (err) {
    // Compensation must never mask the original failure: guard every step and
    // log what it could not undo. On an ID mismatch, the session that leaks is
    // the one the coordinator ACTUALLY created, so delete that one.
    try {
      await deps.sessions.deleteSession({ sessionId: createdSessionId ?? sessionId });
    } catch (delErr) {
      log.error(
        { sessionId: createdSessionId ?? sessionId, err: delErr },
        "task-create: failed to delete session after create failure",
      );
    }
    try {
      await deps.db.transaction(async (tx) => {
        // slack_session has no FK to the task model; the task delete cascades
        // task_session only, so remove the Slack binding explicitly.
        await tx.delete(slackSessionTable).where(eq(slackSessionTable.sessionId, sessionId));
        await tx.delete(taskTable).where(eq(taskTable.id, taskId));
      });
    } catch (dbErr) {
      log.error(
        { taskId, sessionId, err: dbErr },
        "task-create: failed to remove task records after create failure — manual cleanup needed",
      );
    }
    throw err;
  }

  // A just-created session must not be served a stale null from the owner
  // negative-cache window (authz/resolve.ts).
  evictOwnerCacheEntry(sessionId);

  return { taskId, sessionId };
}
