/**
 * Task creation — profile→session compilation + the create-a-task primitive
 * (ADR 0053/0055/0056/0057; extracted in ADR 0060 P2.7, unified here).
 *
 * `createTaskWithSession` is the ONE path that turns a profile into a running
 * agent: compile the CreateSession request, create the upstream session, then
 * persist the `task` + primary `task_session` rows atomically (compensating by
 * deleting the orphan session if the DB write fails). Both the TaskService
 * CreateTask RPC (UI chat tasks) and the external-trigger ThreadControlPlane
 * (ADR 0060 Slack threads) call it, so a triggered session runs with the SAME
 * capabilities/network/secrets/skills as a UI task (no new privilege path) and
 * a session is NEVER created outside the task model.
 *
 * `compileSessionCreateInput` is the intricate inner step — image resolution,
 * harness env (user token + CLI dummy env + profile env + trigger extras),
 * skills union, and the compiled per-session integration policy.
 */

import { ConnectError, Code } from "@connectrpc/connect";
import type { NodePgDatabase } from "drizzle-orm/node-postgres";

import { log as rootLog } from "../log.ts";
import type { ProfileRow, ProfileStore } from "../db/profiles.ts";
import { makePortExposureStore, type PortExposureStore } from "../db/port-exposures.ts";
import type { ImagesClient } from "./profiles.ts";
import { evictOwnerCacheEntry } from "../authz/resolve.ts";
import { task as taskTable, taskSession as taskSessionTable } from "../db/schema.ts";
import * as schema from "../db/schema.ts";
import {
  compileIntegrationPolicy,
  compileCliIntegrations,
  policyHasContent,
  loadRegistry,
  type CustomConnectorSource,
} from "../connectors/registry.ts";

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
  imageUri: string;
  mode: string;
  prompt?: string;
  harnessEnv?: Record<string, string>;
  selectedSkills?: string[];
  capabilities?: string[];
  integrationPolicyJson?: string;
  /** ADR 0062/0063: the selected harness (catalog name) the coordinator mounts
   *  + execs (the proto `CreateSessionRequest.harness`). Resolved from the
   *  per-session override ?? profile ?? deployment default. */
  harness?: string;
}

/** One harness's catalog descriptor (the bits the compiler needs): the model +
 *  effort enums map an option id → the env vars that select it (ADR 0063 §1). */
export interface HarnessDescriptorView {
  /** The env-var names the harness authenticates with (ADR 0063 §1): `userEnv`
   *  is the human credential (per-user token, injected for human tasks);
   *  `orgEnv` is the programmatic credential (B4, host-side resolved). */
  auth?: { userEnv?: string; orgEnv?: string };
  models: Array<{ id: string; default: boolean; env: Record<string, string> }>;
  effort: Array<{ id: string; default: boolean; env: Record<string, string> }>;
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
  /** Resolve the owner's harness token for `envVar` (e.g. CLAUDE_CODE_OAUTH_TOKEN),
   *  or null. Only called when the profile sets includeUserTokens. */
  resolveUserToken: (envVar: string) => Promise<string | null>;
}

export interface SessionCompileOpts {
  prompt?: string;
  /** The task type ("chat" = human/interactive; anything else = programmatic,
   *  e.g. "slack_thread"). Drives the strict-by-run-type credential pick (ADR
   *  0063 B4): human → the harness's `user_env` (per-user token); programmatic →
   *  its `org_env` (org secret, resolved host-side). Default "chat". */
  type?: string;
  /** The creator is a service-account principal (an ADR 0086 API key — e.g. a
   *  `ci-<repo>` CI key). Forces the PROGRAMMATIC credential pick regardless
   *  of task type: a service account has no per-user harness token, so a
   *  "chat" task it creates must still ride `org_env`. */
  programmatic?: boolean;
  /** ADR 0063 B2: per-session override of the profile's default harness / model /
   *  effort. Unset = use the profile's default. */
  harness?: string;
  model?: string;
  effort?: string;
  /** Extra harness env merged LAST (highest precedence) — e.g. the trigger's
   *  ENGRAM_APPEND_SYSTEM_PROMPT (ADR 0060). */
  extraHarnessEnv?: Record<string, string>;
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

  // Strict-by-run-type credentials (ADR 0063 B4): a human (chat) task carries
  // the user's per-user token; a programmatic task carries the org secret. They
  // are mutually exclusive — never both. Run type is the task type AND the
  // principal type: a service-account creator (API key) is programmatic even
  // for a "chat" task — it has no per-user token to inject.
  const isHuman = (opts.type ?? "chat") === "chat" && !opts.programmatic;

  // Harness env, lowest → highest precedence: user token < CLI dummy env <
  // profile env_vars < model env < effort env < trigger extras. NEVER log values.
  const harness: Record<string, string> = {};
  // The human credential env-var name is the selected harness's declared
  // `user_env` (ADR 0063 — no longer the hardcoded CLAUDE_CODE_OAUTH_TOKEN).
  // Injected ONLY for human tasks; programmatic tasks use `org_env` (below).
  const userEnv = descriptor?.auth?.userEnv;
  if (profile.includeUserTokens && isHuman && userEnv) {
    try {
      const userToken = await deps.resolveUserToken(userEnv);
      if (userToken) harness[userEnv] = userToken;
    } catch (secretErr) {
      console.warn("[task-create] user token lookup failed — booting without it", secretErr);
    }
  }
  const registry = await loadRegistry(deps.connectors);
  const cliPlan = compileCliIntegrations(profile.capabilities, registry);
  for (const [k, v] of Object.entries(cliPlan.dummyEnv)) harness[k] = v;
  if (cliPlan.enabled.length > 0) harness.ENGRAM_CLI_INTEGRATIONS = JSON.stringify(cliPlan.enabled);
  for (const [k, v] of Object.entries(profile.envVars)) harness[k] = v;
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
  for (const [k, v] of Object.entries(opts.extraHarnessEnv ?? {})) harness[k] = v;
  const harnessEnv = Object.keys(harness).length > 0 ? harness : undefined;

  // Per-session integration policy (caps + network + secrets), shipped only
  // when it carries content.
  const policy = compileIntegrationPolicy(profile.capabilities, registry, {
    network: profile.network,
    secrets: profile.secrets,
  });
  // ADR 0063 B4: a programmatic task (cron / Slack / API) authenticates the
  // harness with the ORG credential, not a per-user token. The org-secret value
  // never leaves the coordinator (ADR 0057), so we can't read it here — instead
  // append a literal secret-inject naming the org secret (named after the env
  // var by convention; admins create an org secret `ANTHROPIC_API_KEY`). It
  // ships in integration_policy_json and is resolved host-side by
  // resolve_policy_secrets; an unresolvable ref is skipped+warned there (the
  // session still boots).
  const orgEnv = descriptor?.auth?.orgEnv;
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

  // Profile skills ∪ the shared integrations-cli bundle (one dyn_* slot).
  const selectedSkills = [...new Set([...profile.skills, ...cliPlan.bundles])];

  return {
    imageUri: image.imageUri,
    mode: "agent",
    harness: selectedHarness,
    ...(opts.prompt != null ? { prompt: opts.prompt } : {}),
    ...(harnessEnv != null ? { harnessEnv } : {}),
    ...(selectedSkills.length > 0 ? { selectedSkills } : {}),
    ...(profile.capabilities.length > 0 ? { capabilities: profile.capabilities } : {}),
    ...(integrationPolicyJson != null ? { integrationPolicyJson } : {}),
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
  /** Resolve `envVar` for the OWNER (e.g. the Claude OAuth token), or null. */
  secrets: { get(userId: string, envVar: string): Promise<string | null> };
  db: Db;
  /** ADR 0064: port-exposure store for auto-minting `profile.portExposures`.
   *  Defaults to a Drizzle store over `db` when omitted. */
  portExposures?: PortExposureStore;
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
  /** ADR 0063 B2: per-session override of the profile's harness / model / effort. */
  harness?: string;
  model?: string;
  effort?: string;
  /** Type-specific trigger ref recorded on the task row (operator-visible). */
  source?: Record<string, unknown>;
  /** Extra harness env merged LAST — e.g. the trigger's
   *  ENGRAM_APPEND_SYSTEM_PROMPT (ADR 0060). */
  extraHarnessEnv?: Record<string, string>;
}

export interface CreatedTask {
  taskId: string;
  sessionId: string;
}

/**
 * Create a task and its primary session in one atomic operation. Loads the
 * active profile (NotFound if missing/archived), compiles the CreateSession
 * request, creates the upstream session, then persists the task + task_session
 * rows in one transaction. If the DB write fails the orphan session is deleted
 * (best-effort) before rethrowing so a retry starts clean.
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

  const sessionInput = await compileSessionCreateInput(
    profile,
    {
      images: deps.images,
      connectors: deps.connectors,
      harnessCatalog: deps.harnessCatalog,
      resolveUserToken: (envVar) => deps.secrets.get(params.ownerUserId, envVar),
    },
    {
      type: params.type,
      ...(params.ownerIsServiceAccount ? { programmatic: true } : {}),
      ...(params.prompt != null ? { prompt: params.prompt } : {}),
      ...(params.harness != null ? { harness: params.harness } : {}),
      ...(params.model != null ? { model: params.model } : {}),
      ...(params.effort != null ? { effort: params.effort } : {}),
      ...(params.extraHarnessEnv ? { extraHarnessEnv: params.extraHarnessEnv } : {}),
    },
  );

  const created = await deps.sessions.createSession(sessionInput);

  const taskId = crypto.randomUUID();
  try {
    await deps.db.transaction(async (tx) => {
      await tx.insert(taskTable).values({
        id: taskId,
        type: params.type,
        // Initial (default) title = the truncated prompt. A harness AI title
        // later overrides it (via task.suggested_title + the buildTask
        // derivation) unless the user sets a sticky custom title. A caller-
        // supplied `params.title` still wins when present (e.g. a trigger that
        // names the task explicitly).
        title: params.title ?? truncatePrompt(params.prompt),
        status: "open",
        createdByUserId: params.ownerUserId,
        source: params.source ?? {},
      });
      await tx.insert(taskSessionTable).values({
        taskId,
        sessionId: created.sessionId,
        role: "primary",
        profileId: profile.id,
      });
    });
  } catch (err) {
    // Compensate: drop the orphan session so a retry starts clean.
    try {
      await deps.sessions.deleteSession({ sessionId: created.sessionId });
    } catch (delErr) {
      log.error(
        { sessionId: created.sessionId, err: delErr },
        "task-create: failed to delete orphan session after task-persist failure",
      );
    }
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
          sessionId: created.sessionId,
          port,
          label: "",
          ownerUserId: params.ownerUserId,
          visibility: "private",
        });
      } catch (e) {
        log.warn(
          { sessionId: created.sessionId, port, err: e },
          "task-create: auto-expose port failed (continuing)",
        );
      }
    }
  }

  // A just-created session must not be served a stale null from the owner
  // negative-cache window (authz/resolve.ts).
  evictOwnerCacheEntry(created.sessionId);

  return { taskId, sessionId: created.sessionId };
}
