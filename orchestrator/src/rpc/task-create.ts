/**
 * Task creation — profile→session compilation + the create-a-task primitive
 * (ADR 0052/0055/0056/0057; extracted in ADR 0060 P2.7, unified here).
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
import type { ImagesClient } from "./profiles.ts";
import { CLAUDE_OAUTH_ENV_VAR } from "../db/user-secrets.ts";
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
}

export interface SessionCompileDeps {
  images: ImagesClient;
  connectors: CustomConnectorSource;
  /** Resolve the owner's harness token for `envVar` (e.g. CLAUDE_CODE_OAUTH_TOKEN),
   *  or null. Only called when the profile sets includeUserTokens. */
  resolveUserToken: (envVar: string) => Promise<string | null>;
}

export interface SessionCompileOpts {
  prompt?: string;
  /** Extra harness env merged LAST (highest precedence) — e.g. the trigger's
   *  ENGRAM_APPEND_SYSTEM_PROMPT (ADR 0060). */
  extraHarnessEnv?: Record<string, string>;
}

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

  // Harness env, lowest → highest precedence: user token < CLI dummy env <
  // profile env_vars < trigger extras. NEVER log values.
  const harness: Record<string, string> = {};
  if (profile.includeUserTokens) {
    try {
      const userToken = await deps.resolveUserToken(CLAUDE_OAUTH_ENV_VAR);
      if (userToken) harness[CLAUDE_OAUTH_ENV_VAR] = userToken;
    } catch (secretErr) {
      console.warn("[task-create] user token lookup failed — booting without it", secretErr);
    }
  }
  const registry = await loadRegistry(deps.connectors);
  const cliPlan = compileCliIntegrations(profile.capabilities, registry);
  for (const [k, v] of Object.entries(cliPlan.dummyEnv)) harness[k] = v;
  if (cliPlan.enabled.length > 0) harness.ENGRAM_CLI_INTEGRATIONS = JSON.stringify(cliPlan.enabled);
  for (const [k, v] of Object.entries(profile.envVars)) harness[k] = v;
  for (const [k, v] of Object.entries(opts.extraHarnessEnv ?? {})) harness[k] = v;
  const harnessEnv = Object.keys(harness).length > 0 ? harness : undefined;

  // Per-session integration policy (caps + network + secrets), shipped only
  // when it carries content.
  const policy = compileIntegrationPolicy(profile.capabilities, registry, {
    network: profile.network,
    secrets: profile.secrets,
  });
  const integrationPolicyJson = policyHasContent(policy) ? JSON.stringify(policy) : undefined;

  // Profile skills ∪ the shared integrations-cli bundle (one dyn_* slot).
  const selectedSkills = [...new Set([...profile.skills, ...cliPlan.bundles])];

  return {
    imageUri: image.imageUri,
    mode: "agent",
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
  sessions: TaskSessionsClient;
  /** Resolve `envVar` for the OWNER (e.g. the Claude OAuth token), or null. */
  secrets: { get(userId: string, envVar: string): Promise<string | null> };
  db: Db;
}

export interface CreateTaskParams {
  /** Task type: "chat" (UI) | "slack_thread" (ADR 0060 trigger) | … */
  type: string;
  /** The engrams user who owns the task (createdByUserId → the CASL subject). */
  ownerUserId: string;
  /** The profile to start from; must be active (else NotFound). */
  profileId: string;
  title?: string | null;
  prompt?: string;
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
      resolveUserToken: (envVar) => deps.secrets.get(params.ownerUserId, envVar),
    },
    {
      ...(params.prompt != null ? { prompt: params.prompt } : {}),
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
        title: params.title ?? null,
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

  // A just-created session must not be served a stale null from the owner
  // negative-cache window (authz/resolve.ts).
  evictOwnerCacheEntry(created.sessionId);

  return { taskId, sessionId: created.sessionId };
}
