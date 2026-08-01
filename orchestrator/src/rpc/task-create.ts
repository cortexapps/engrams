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
import { makePortExposureStore, type PortExposureStore } from "../db/port-exposures.ts";
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
} from "../connectors/registry.ts";
import { compileToolManifest } from "../tools/manifest.ts";
import { tools as productionTools, type ToolRegistry } from "../tools/registry.ts";
import { BASE_SYSTEM_PROMPT } from "../prompts/base.ts";
import { OauthSubjectKind } from "../gen/engram/app/v1/oauth_pb.ts";
import { oauthCredential as defaultOAuthCredential } from "../control-plane/client.ts";
import {
  makeIntegrationConnectionStore,
  type IntegrationConnectionStore,
} from "../db/integration-connections.ts";
import {
  makeProfileLaunchGrantStore,
  type ProfileLaunchGrantStore,
} from "../db/profile-launch-grants.ts";
import {
  grantsToCapabilities,
  legacyCapabilityGrant,
  resolveIntegrationGrants,
} from "../integrations/grants.ts";
import { appendGooglePolicy } from "../integrations/google-policy.ts";

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
  models: Array<{ id: string; default: boolean; env: Record<string, string> }>;
  effort: Array<{ id: string; default: boolean; env: Record<string, string> }>;
  /** ADR 0107: declared session modes (pure declaration — no env). */
  modes?: Array<{ id: string; default: boolean }>;
}
export interface HarnessCatalogClient {
  listHarnesses(req: Record<string, never>): Promise<{
    harnesses: Array<{ name: string; descriptor?: HarnessDescriptorView }>;
  }>;
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
  /** Resolve whether the human owner has a live provider connection. */
  hasOAuthCredential?: (provider: string) => Promise<boolean>;
  oauthSubject?: { kind: OauthSubjectKind; id: string };
  connections: IntegrationConnectionStore;
}

export interface SessionCompileOpts {
  prompt?: string;
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
  effort?: string;
  /** Extra harness env merged LAST (highest precedence) — e.g. the trigger's
   *  ENGRAM_APPEND_SYSTEM_PROMPT (ADR 0060). */
  extraHarnessEnv?: Record<string, string>;
  /** The initiating human's identity for git commit attribution (ADR 0031 §7),
   *  stamped as ENGRAM_USER_NAME/ENGRAM_USER_EMAIL — the guest writes them into
   *  /etc/gitconfig's [user] block so in-session commits are authored by the
   *  human who started the session. Omit for service-account owners. */
  owner?: { name: string; email: string };
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

  // ADR 0062/0063: resolve the effective harness/model/effort (per-session
  // override < profile default < deployment/descriptor default).
  const selectedHarness = opts.harness ?? profile.harness ?? DEFAULT_HARNESS;
  const { harnesses } = await deps.harnessCatalog.listHarnesses({});
  const descriptor = harnesses.find((h) => h.name === selectedHarness)?.descriptor;

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
  const isHuman = !opts.programmatic;

  // General harness env, lowest → highest precedence: other user tokens < CLI
  // dummy env < profile env_vars < model env < effort env < git attribution <
  // trigger extras. The selected harness's principal credential is applied
  // LAST below, outside this precedence chain. NEVER log values.
  const harness: Record<string, string> = {};
  // The human credential env-var name is the selected harness's declared
  // `user_env` (ADR 0063 — no longer the hardcoded CLAUDE_CODE_OAUTH_TOKEN).
  const userEnv = descriptor?.auth?.userEnv;
  const userOauth = descriptor?.auth?.userOauth;
  const orgEnv = descriptor?.auth?.orgEnv;
  let humanUserToken: string | undefined;
  let oauthCredential: SessionCreateInput["oauthCredential"];
  if (isHuman) {
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
  const resolvedProfileGrants = await resolveIntegrationGrants(
    profile.integrationGrants,
    deps.connections,
  );
  const profileCapabilities = grantsToCapabilities(resolvedProfileGrants);
  const effectiveGrants = opts.capabilityOverride !== undefined
    ? opts.capabilityOverride.map(legacyCapabilityGrant)
    : [
        ...profile.integrationGrants,
        ...(opts.extraCapabilities ?? []).map(legacyCapabilityGrant),
      ];
  const resolvedEffectiveGrants = await resolveIntegrationGrants(
    effectiveGrants,
    deps.connections,
  );
  const disabledConnection = resolvedEffectiveGrants.find(({ connection }) => !connection.enabled);
  if (disabledConnection) {
    throw new ConnectError(
      `integration connection "${disabledConnection.connection.alias}" is disabled`,
      Code.FailedPrecondition,
    );
  }
  const hasGoogleCloud = resolvedEffectiveGrants.some(({ connection }) => connection.provider === "gcp");
  const capabilities = grantsToCapabilities(resolvedEffectiveGrants);
  // A capability override is the complete session authority and therefore
  // also owns its CLI/tool surface. Without one, preserve the narrower
  // profile-owned surface: extra integration grants do not add model tools.
  const surfacedCapabilities = opts.capabilityOverride !== undefined
    ? capabilities
    : profileCapabilities;
  const cliPlan = compileCliIntegrations(surfacedCapabilities, registry);
  for (const [k, v] of Object.entries(cliPlan.dummyEnv)) harness[k] = v;
  const enabledCli = [
    ...cliPlan.enabled,
    ...(hasGoogleCloud
      ? [{
          provider: "gcp",
          displayName: "Google Cloud",
          bins: ["gcloud"],
          doc: "Use brokered metadata ADC. Do not log in or create credentials.",
        }]
      : []),
  ];
  if (enabledCli.length > 0) harness.ENGRAM_CLI_INTEGRATIONS = JSON.stringify(enabledCli);
  const toolManifest = compileToolManifest(
    deps.toolRegistry ?? productionTools,
    surfacedCapabilities,
  );
  if (toolManifest.length > 0) harness.ENGRAM_TOOLS = JSON.stringify(toolManifest);
  if (!opts.dropProfileSecretsAndEnv) {
    for (const [k, v] of Object.entries(profile.envVars)) harness[k] = v;
  }
  // ADR 0063: the selected model/effort map to env vars via the harness
  // descriptor (an explicit picker wins over a stale ANTHROPIC_MODEL in env_vars).
  if (descriptor) {
    const modelId =
      opts.model ?? profile.model ?? descriptor.models.find((m) => m.default)?.id ?? descriptor.models[0]?.id;
    const effortId =
      opts.effort ?? profile.effort ?? descriptor.effort.find((e) => e.default)?.id ?? descriptor.effort[0]?.id;
    const modelEnv = descriptor.models.find((m) => m.id === modelId)?.env ?? {};
    const effortEnv = descriptor.effort.find((e) => e.id === effortId)?.env ?? {};
    for (const [k, v] of Object.entries(modelEnv)) harness[k] = v;
    for (const [k, v] of Object.entries(effortEnv)) harness[k] = v;
  }
  // ADR 0031 §7: git commit attribution — the initiating human authors the
  // in-session commits (the guest turns these into /etc/gitconfig's [user]
  // block). Orchestrator-authoritative, so it beats profile env_vars.
  if (opts.owner) {
    harness.ENGRAM_USER_NAME = opts.owner.name;
    harness.ENGRAM_USER_EMAIL = opts.owner.email;
  }
  for (const [k, v] of Object.entries(opts.extraHarnessEnv ?? {})) harness[k] = v;
  harness.ENGRAM_APPEND_SYSTEM_PROMPT = [
    harness.ENGRAM_APPEND_SYSTEM_PROMPT,
    BASE_SYSTEM_PROMPT,
  ].filter(Boolean).join("\n\n");
  // ADR 0097: the browser bundle carries a local image-observation tool. It
  // is harness-native (not a connector capability) and is enabled only when
  // the corresponding skill is mounted into this session.
  if (hasGoogleCloud) {
    harness.GCE_METADATA_HOST = "169.254.169.254";
    harness.GCE_METADATA_IP = "169.254.169.254";
    harness.CLOUDSDK_CORE_CHECK_GCE_METADATA = "true";
  }
  const selectedSkills = [...new Set([
    ...profile.skills,
    ...cliPlan.bundles,
    ...(hasGoogleCloud ? ["integrations-cli"] : []),
  ])];
  if (selectedSkills.includes("browser")) harness.ENGRAM_BROWSER_VIEW_ENABLED = "1";
  else delete harness.ENGRAM_BROWSER_VIEW_ENABLED;

  // The selected harness's credential is PRINCIPAL-authoritative, not profile
  // configuration. A human run always gets exactly its required per-user
  // `user_env`, applied after every configurable env layer so an admin profile,
  // model, or trigger cannot replace it. A programmatic run gets `org_env`
  // exclusively from the host-side org-secret policy below, so strip both auth
  // names from the orchestrator-provided env. This also preserves the strict
  // invariant that a session never receives both credential tiers.
  if (isHuman) {
    if (orgEnv) delete harness[orgEnv];
    if (userEnv && humanUserToken !== undefined) harness[userEnv] = humanUserToken;
  } else {
    if (userEnv) delete harness[userEnv];
    if (orgEnv) delete harness[orgEnv];
  }
  const harnessEnv = Object.keys(harness).length > 0 ? harness : undefined;

  // Per-session integration policy (caps + network + secrets), shipped only
  // when it carries content.
  const policy = compileIntegrationPolicy(capabilities, registry, {
    network: opts.networkOverride ?? profile.network,
    secrets: opts.dropProfileSecretsAndEnv ? [] : profile.secrets,
  });
  appendGooglePolicy(policy, resolvedEffectiveGrants);
  policy.google_adc = hasGoogleCloud;
  // ADR 0063 B4: a programmatic task (cron / Slack / API) authenticates the
  // harness with the ORG credential, not a per-user token. The org-secret value
  // never leaves the coordinator (ADR 0057), so we can't read it here — instead
  // append a literal secret-inject naming the org secret (named after the env
  // var by convention; admins create an org secret `ANTHROPIC_API_KEY`). It
  // ships in integration_policy_json and is resolved host-side by
  // resolve_policy_secrets; an unresolvable ref is skipped+warned there (the
  // session still boots).
  if (!isHuman && orgEnv) {
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
    ...(opts.prompt != null ? { prompt: opts.prompt } : {}),
    ...(opts.harnessMode != null ? { harnessMode: opts.harnessMode } : {}),
    ...(harnessEnv != null ? { harnessEnv } : {}),
    ...(oauthCredential != null ? { oauthCredential } : {}),
    ...(selectedSkills.length > 0 ? { selectedSkills } : {}),
    ...(capabilities.length > 0 ? { capabilities } : {}),
    ...(integrationPolicyJson != null ? { integrationPolicyJson } : {}),
    integrationGrants: effectiveGrants,
    integrationConnections: [...new Map(
      resolvedEffectiveGrants.map(({ connection }) => [connection.id, {
        id: connection.id,
        alias: connection.alias,
        provider: connection.provider,
        displayName: connection.displayName,
        config: structuredClone(connection.config),
      } satisfies IntegrationConnectionSnapshot]),
    ).values()],
  };
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
      credentials: Array<{ provider: string; connected: boolean }>;
    }>;
  };
  db: Db;
  /** ADR 0064: port-exposure store for auto-minting `profile.portExposures`.
   *  Defaults to a Drizzle store over `db` when omitted. */
  portExposures?: PortExposureStore;
  /** ADR 0031 §7: owner identity lookup for git commit attribution.
   *  Defaults to a Drizzle store over `db` when omitted. */
  users?: UserIdentityStore;
  connections?: IntegrationConnectionStore;
  launchGrants?: ProfileLaunchGrantStore;
}

export interface CreateTaskParams {
  /** Task type: "chat" (UI) | "slack_thread" (ADR 0060 trigger) | … */
  type: string;
  /** The engrams user who owns the task (createdByUserId → the CASL subject). */
  ownerUserId: string;
  /** The owner is a service-account principal (API key) — forces the
   *  programmatic (org-credential) compile path; see SessionCompileOpts. */
  ownerIsServiceAccount?: boolean;
  /** Administrators have implicit access to restricted profiles. */
  ownerIsAdmin?: boolean;
  /** The profile to start from; must be active (else NotFound). */
  profileId: string;
  title?: string | null;
  prompt?: string;
  /** ADR 0063 B2: per-session override of the profile's harness / model / effort. */
  harness?: string;
  model?: string;
  effort?: string;
  /** ADR 0107: session mode for the initial prompt (e.g. "plan"). */
  harnessMode?: string;
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
  ownerUserId?: string;
  /** Stable non-human principal used for restricted-profile launch grants. */
  launchPrincipalId?: string;
  prompt?: string;
  /** ADR 0107: session mode for the initial prompt (e.g. "plan"). */
  harnessMode?: string;
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
  /** Administrators have implicit access to restricted profiles. */
  ownerIsAdmin?: boolean;
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
export async function registerSessionListener(
  db: Db,
  sessionId: string,
): Promise<void> {
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
  const launchPrincipalId = params.ownerUserId ?? params.launchPrincipalId;
  if (
    profile.launchAccess === "restricted" &&
    !params.ownerIsAdmin &&
    (launchPrincipalId === undefined ||
      !(await (deps.launchGrants ?? makeProfileLaunchGrantStore(deps.db)).canLaunch(
        profile.id,
        launchPrincipalId,
      )))
  ) {
    throw new ConnectError("profile launch is not granted", Code.PermissionDenied);
  }

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
          }),
      connections: deps.connections ?? makeIntegrationConnectionStore(deps.db),
    },
    {
      // An automation-owned review task has no human token; use the harness's
      // programmatic credential while still creating the session promptless.
      ...(params.ownerUserId === undefined ? { programmatic: true } : {}),
      ...(params.prompt != null ? { prompt: params.prompt } : {}),
      ...(params.harnessMode != null ? { harnessMode: params.harnessMode } : {}),
      ...(params.extraCapabilities ? { extraCapabilities: params.extraCapabilities } : {}),
      ...(params.capabilityOverride !== undefined
        ? { capabilityOverride: params.capabilityOverride }
        : {}),
      ...(params.networkOverride !== undefined
        ? { networkOverride: params.networkOverride }
        : {}),
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
      ...(launchPrincipalId ? { integrationPrincipalId: launchPrincipalId } : {}),
    });
  });

  try {
    // When prompt is omitted (as it is for the finder), the session boots idle
    // so deterministic bootstrap can finish before SendPrompt wakes it.
    const created = await deps.sessions.createSession(sessionInput);
    if (created.sessionId !== sessionId) {
      throw new Error("coordinator returned a different reserved session ID");
    }
    if (params.registerListener === true) {
      await deps.db.transaction(async (tx) => {
        await tx.insert(sessionListenerTable).values({ sessionId });
      });
    }
  } catch (err) {
    try {
      await deps.sessions.deleteSession({ sessionId });
    } catch (delErr) {
      log.error(
        { sessionId, err: delErr },
        "task-create: failed to delete reserved session after create failure",
      );
    }
    await deps.db.transaction(async (tx) => {
      await tx
        .delete(taskSessionTable)
        .where(and(eq(taskSessionTable.taskId, params.taskId), eq(taskSessionTable.sessionId, sessionId)));
    });
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
  if (
    profile.launchAccess === "restricted" &&
    !params.ownerIsAdmin &&
    !(await (deps.launchGrants ?? makeProfileLaunchGrantStore(deps.db)).canLaunch(
      profile.id,
      params.ownerUserId,
    ))
  ) {
    throw new ConnectError("profile launch is not granted", Code.PermissionDenied);
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
      connections: deps.connections ?? makeIntegrationConnectionStore(deps.db),
    },
    {
      ...(params.ownerIsServiceAccount ? { programmatic: true } : {}),
      ...(params.prompt != null ? { prompt: params.prompt } : {}),
      ...(params.harness != null ? { harness: params.harness } : {}),
      ...(params.model != null ? { model: params.model } : {}),
      ...(params.effort != null ? { effort: params.effort } : {}),
      ...(params.harnessMode != null ? { harnessMode: params.harnessMode } : {}),
      ...(params.extraHarnessEnv ? { extraHarnessEnv: params.extraHarnessEnv } : {}),
      ...(owner ? { owner } : {}),
    },
  );

  const taskId = deps.newTaskId?.() ?? crypto.randomUUID();
  const sessionId = deps.newSessionId?.() ?? crypto.randomUUID();
  sessionInput.requestedSessionId = sessionId;

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
    });
    await tx.insert(taskSessionTable).values({
      taskId,
      sessionId,
      role: "primary",
      profileId: profile.id,
      capabilities: sessionInput.capabilities ?? [],
      integrationGrants: sessionInput.integrationGrants ?? [],
      integrationConnections: sessionInput.integrationConnections ?? [],
      integrationPrincipalId: params.ownerUserId,
    });
    if (params.slackThreadWorkflowId !== undefined) {
      await tx.insert(slackSessionTable).values({
        sessionId,
        threadWfId: params.slackThreadWorkflowId,
      });
    }
  });

  try {
    const created = await deps.sessions.createSession(sessionInput);
    if (created.sessionId !== sessionId) {
      throw new Error("coordinator returned a different reserved session ID");
    }
    await deps.db.transaction(async (tx) => {
      await tx.insert(sessionListenerTable).values({ sessionId });
    });
  } catch (err) {
    try {
      await deps.sessions.deleteSession({ sessionId });
    } catch (delErr) {
      log.error(
        { sessionId, err: delErr },
        "task-create: failed to delete reserved session after create failure",
      );
    }
    await deps.db.transaction(async (tx) => {
      await tx.delete(taskTable).where(eq(taskTable.id, taskId));
    });
    throw err;
  }

  // ADR 0064: auto-mint one private port-exposure per port the profile declares.
  // Best-effort — an exposure failure must NOT fail the task (the session is
  // already live + persisted); log and continue so the rest still land.
  if (profile.portExposures.length > 0) {
    const store = deps.portExposures ?? makePortExposureStore(deps.db);
    for (const port of profile.portExposures) {
      try {
        await store.createOrGet({
          sessionId,
          port,
          label: "",
          ownerUserId: params.ownerUserId,
          visibility: "private",
        });
      } catch (e) {
        log.warn(
          { sessionId, port, err: e },
          "task-create: auto-expose port failed (continuing)",
        );
      }
    }
  }

  // A just-created session must not be served a stale null from the owner
  // negative-cache window (authz/resolve.ts).
  evictOwnerCacheEntry(sessionId);

  return { taskId, sessionId };
}
