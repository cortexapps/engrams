/**
 * Native TaskService implementation (ADR 0051 §3, Task 19).
 *
 * This service is orchestrator-native — it is registered on the same
 * ConnectRouter as the generic passthrough, but is NEVER proxied to the
 * control plane. The web client sees one uniform generated API.
 *
 * Session status mapping (control-plane → task status):
 *   pending | created | guest_ready | active | idle | evacuating | evicting
 *     → "working"   (session is alive / in progress)
 *   completed
 *     → "done"      (session ran to completion)
 *   failed | dead | host_lost
 *     → "failed"    (session is unrecoverable)
 *   (unrecognised / blank)
 *     → keep task's own persisted status ("open" at create, unchanged)
 *
 * Unattributed sessions (anti-attribution / synthetic admin rows):
 *   Control-plane sessions with NO task_session row in the orchestrator DB are
 *   surfaced as synthetic admin-only Task rows in ListTasks. Their id follows
 *   the pattern "unattributed-<sessionId>" and they carry type="chat",
 *   createdByUserId=null, title=null, status derived from session state.
 *   Members never see these rows (ability.can('read', subject('Task', {createdByUserId: null}))
 *   is false for non-admin). This design lets an admin see orphan sessions
 *   that pre-date the task model or were created outside the orchestrator.
 *
 * Anti-enumeration (GetTask / DeleteTask):
 *   A task that exists but is owned by another user returns NotFound (not
 *   PermissionDenied) so callers cannot infer whether a given task id exists.
 *   This matches the passthrough gate's convention (ADR §6).
 *
 * Compensation (createTask):
 *   If upstream CreateSession succeeds but the orchestrator DB insert fails,
 *   we delete the upstream session before rethrowing so no orphan sessions
 *   accumulate. This is a best-effort synchronous rollback (chat tasks have no
 *   DBOS; compensating delete failure is logged but not fatal).
 *
 * Injectable deps:
 *   sessions, secrets, db are injectable for tests. The real singletons are
 *   used by default. getSession (better-auth) is also injectable.
 */

import { ConnectError, Code } from "@connectrpc/connect";
import type { ConnectRouter, HandlerContext } from "@connectrpc/connect";
import { subject } from "@casl/ability";
import { eq, inArray } from "drizzle-orm";
import type { NodePgDatabase } from "drizzle-orm/node-postgres";

import { TaskService } from "../gen/engram/app/v1/task_pb.ts";
import type { Task, TaskSessionRef } from "../gen/engram/app/v1/task_pb.ts";
import type { Session } from "../gen/engram/app/v1/session_pb.ts";

import { abilityFor } from "../authz/ability.ts";
import { evictOwnerCacheEntry } from "../authz/resolve.ts";
import { auth } from "../auth/better-auth.ts";
import { getDb } from "../db/client.ts";
import { task as taskTable, taskSession as taskSessionTable } from "../db/schema.ts";
import * as schema from "../db/schema.ts";
import { sessions as defaultSessions, images as defaultImages } from "../control-plane/client.ts";
import {
  makeUserSecretStore,
  type UserSecretStore,
  CLAUDE_OAUTH_ENV_VAR,
} from "../db/user-secrets.ts";
import { makeProfileStore, type ProfileStore } from "../db/profiles.ts";
import type { ImagesClient } from "./profiles.ts";
import {
  compileIntegrationPolicy,
  compileCliIntegrations,
  compileMcpIntegrations,
  policyHasContent,
  loadRegistry,
  type CustomConnectorSource,
} from "../connectors/registry.ts";
import { makeConnectorStore } from "../db/connectors.ts";

// Re-export ImagesClient so downstream modules (image-guard, tests) can import
// it from tasks.ts. The canonical declaration lives in rpc/profiles.ts.
export type { ImagesClient } from "./profiles.ts";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Subset of SessionService client used by TaskService. */
export interface SessionsClient {
  createSession(req: {
    imageUri: string;
    mode: string;
    prompt?: string;
    // ADR 0051 Drip A: orchestrator-resolved harness identity env (e.g.
    // CLAUDE_CODE_OAUTH_TOKEN). The coordinator injects + persists it.
    harnessEnv?: Record<string, string>;
    // ADR 0055: profile-selected skill bundle names; the coordinator resolves
    // them to reserved-slot mounts at boot.
    selectedSkills?: string[];
    // ADR 0056: profile-granted "provider:action[@resource]" capabilities; the
    // coordinator binds them to the session (+ later clamps).
    capabilities?: string[];
    // ADR 0056 (B′): the per-session IntegrationPolicy the orchestrator compiled
    // from the profile's capabilities + connector config (a JSON string). The
    // coordinator persists it + resolves its secret_refs host-side.
    integrationPolicyJson?: string;
  }): Promise<{ sessionId: string; status: string; imageVersion: string; kind: string }>;
  listSessions(req: Record<string, never>): Promise<{ sessions: Array<{ session?: Session | undefined }> }>;
  getSession(req: { sessionId: string }): Promise<{ session?: Session | undefined }>;
  deleteSession(req: { sessionId: string }): Promise<unknown>;
}

/** Drizzle DB type used by TaskService. */
export type Db = NodePgDatabase<typeof schema>;

/** Injectable better-auth getSession function. */
export type GetSession = (
  headers: Headers,
) => Promise<{
  user: { id: string; role?: string | null; email?: string | null };
} | null>;

/** Dependency bag for registerTasks — all optional, real singletons used by default. */
export interface TaskDeps {
  getSession?: GetSession;
  sessions?: SessionsClient;
  /** Per-user KEK-sealed session secret store (ADR 0051 Drip A). */
  secrets?: UserSecretStore;
  /** Admin-curated session profiles (ADR 0052). */
  profiles?: ProfileStore;
  /** Enabled-image catalog client (ADR 0052) — resolves image_id → image_uri. */
  images?: ImagesClient;
  /** Connector catalog (ADR 0057) — custom connectors merged with built-in seeds. */
  connectors?: CustomConnectorSource;
  db?: Db;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Extract request headers from a HandlerContext as a plain Headers object. */
function headersOf(ctx: HandlerContext): Headers {
  return ctx.requestHeader;
}

/**
 * Resolve the caller's better-auth session from the request headers.
 * Throws Unauthenticated if the session is absent.
 */
async function requireUser(
  ctx: HandlerContext,
  getSession: GetSession,
): Promise<{ id: string; role: string }> {
  const session = await getSession(headersOf(ctx));
  if (!session) {
    throw new ConnectError("unauthenticated", Code.Unauthenticated);
  }
  return { id: session.user.id, role: session.user.role ?? "user" };
}

/**
 * Map a control-plane session status string to a task status string.
 *
 * Session status ∈ { pending, created, guest_ready, active, idle,
 *                    evacuating, evicting, completed, failed, dead, host_lost }
 *
 * Mapping (documented in module JSDoc above):
 *   pending | created | guest_ready | active | idle | evacuating | evicting → working
 *   completed → done
 *   failed | dead | host_lost → failed
 *   (anything else) → null (caller keeps persisted task status)
 */
function sessionStatusToTaskStatus(sessionStatus: string): string | null {
  switch (sessionStatus) {
    case "pending":
    case "created":
    case "guest_ready":
    case "active":
    case "idle":
    case "evacuating":
    case "evicting":
      return "working";
    case "completed":
      return "done";
    case "failed":
    case "dead":
    case "host_lost":
      return "failed";
    default:
      return null;
  }
}

/**
 * Build a proto Task from a DB row + a map of sessionId → live Session.
 * The sessionMap may be empty (no upstream session found → session field unset).
 */
function buildTask(
  row: {
    id: string;
    type: string;
    title: string | null;
    status: string;
    createdByUserId: string | null;
    source: unknown;
    createdAt: Date;
  },
  sessionRefs: Array<{ sessionId: string; role: string | null; profileId: string | null }>,
  sessionMap: Map<string, Session>,
  profileMap: Map<string, { id: string; name: string; icon: string; archived: boolean; imageUri: string }>,
): Task {
  // Derive status from the primary session's live state (if available).
  let status = row.status;
  const primaryRef = sessionRefs.find((r) => r.role === "primary") ?? sessionRefs[0];
  if (primaryRef) {
    const liveSession = sessionMap.get(primaryRef.sessionId);
    if (liveSession) {
      const mapped = sessionStatusToTaskStatus(liveSession.status);
      if (mapped !== null) status = mapped;
    }
  }

  const sessions: TaskSessionRef[] = sessionRefs.map((ref) => {
    const liveSession = sessionMap.get(ref.sessionId);
    const snap = ref.profileId != null ? profileMap.get(ref.profileId) : undefined;
    return {
      sessionId: ref.sessionId,
      ...(ref.role != null ? { role: ref.role } : {}),
      ...(liveSession != null ? { session: liveSession } : {}),
      ...(snap != null ? { profile: snap } : {}),
    } as TaskSessionRef;
  });

  return {
    id: row.id,
    type: row.type,
    ...(row.title != null ? { title: row.title } : {}),
    status,
    ...(row.createdByUserId != null ? { createdByUserId: row.createdByUserId } : {}),
    sourceJson: JSON.stringify(row.source ?? {}),
    sessions,
    createdAt: row.createdAt.toISOString(),
  } as Task;
}

/**
 * Build a synthetic admin-only Task row for an unattributed control-plane
 * session (a session with no matching task_session row).
 *
 * id: "unattributed-<sessionId>"
 * type: "chat"
 * createdByUserId: null (unset)
 * title: null (unset)
 * status: derived from session state
 * sessions: single TaskSessionRef with role "primary"
 * createdAt: session.createdAt
 */
function buildUnattributedTask(sess: Session): Task {
  const status = sessionStatusToTaskStatus(sess.status) ?? "open";
  return {
    id: `unattributed-${sess.id}`,
    type: "chat",
    status,
    sourceJson: "{}",
    sessions: [{ sessionId: sess.id, role: "primary", session: sess } as TaskSessionRef],
    createdAt: sess.createdAt,
  } as Task;
}

/** Resolve { profileId } → snapshot for the given refs (one images call + one profile query). */
export async function buildProfileMap(
  refs: Array<{ profileId: string | null }>,
  profiles: ProfileStore,
  imagesClient: ImagesClient,
): Promise<Map<string, { id: string; name: string; icon: string; archived: boolean; imageUri: string }>> {
  const ids = [...new Set(refs.map((r) => r.profileId).filter((x): x is string => x != null))];
  const out = new Map<string, { id: string; name: string; icon: string; archived: boolean; imageUri: string }>();
  if (ids.length === 0) return out;
  // The image catalog lives on the control plane and may be transiently
  // unavailable. Reads must stay best-effort: a catalog failure must not take
  // down ListTasks/GetTask — the profile snapshot is still returned and only
  // imageUri degrades to "". The profile store is the orchestrator's own DB, so
  // its failure remains a hard error.
  const catalogPromise = imagesClient
    .listEnabledImages({})
    .then((catalog) => new Map(catalog.images.map((i) => [i.id, i.imageUri])))
    .catch(() => new Map<string, string>());
  const [rows, uriById] = await Promise.all([profiles.getByIds(ids), catalogPromise]);
  for (const p of rows) {
    out.set(p.id, {
      id: p.id, name: p.name, icon: p.icon,
      archived: p.deletedAt != null,
      imageUri: uriById.get(p.imageId) ?? "",
    });
  }
  return out;
}

// ---------------------------------------------------------------------------
// Core loader
// ---------------------------------------------------------------------------

/**
 * Load a single task by id, joining with live session state.
 * Returns the Task proto or throws NotFound.
 */
async function loadTask(
  taskId: string,
  db: Db,
  sessionsClient: SessionsClient,
  profiles: ProfileStore,
  imagesClient: ImagesClient,
): Promise<Task> {
  const db_ = db;

  // Fetch task row.
  const taskRows = await db_
    .select()
    .from(taskTable)
    .where(eq(taskTable.id, taskId))
    .limit(1);

  if (taskRows.length === 0) {
    throw new ConnectError("not found", Code.NotFound);
  }
  const taskRow = taskRows[0]!;

  // Fetch task_session rows.
  const sessionRefRows = await db_
    .select()
    .from(taskSessionTable)
    .where(eq(taskSessionTable.taskId, taskId));

  // Fetch live session state for each session.
  const sessionMap = new Map<string, Session>();
  for (const ref of sessionRefRows) {
    try {
      const resp = await sessionsClient.getSession({ sessionId: ref.sessionId });
      if (resp.session) {
        sessionMap.set(ref.sessionId, resp.session);
      }
    } catch {
      // Best-effort: upstream session may be gone; omit from map.
    }
  }

  const profileMap = await buildProfileMap(sessionRefRows, profiles, imagesClient);
  return buildTask(taskRow, sessionRefRows, sessionMap, profileMap);
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/**
 * Register the native TaskService on the ConnectRouter.
 *
 * All deps are injectable for tests; real singletons are used when omitted.
 */
export function registerTasks(router: ConnectRouter, deps?: TaskDeps): void {
  const getSession: GetSession =
    deps?.getSession ??
    ((headers) =>
      auth.api.getSession({ headers } as Parameters<typeof auth.api.getSession>[0]));

  const sessionsClient: SessionsClient = deps?.sessions ?? (defaultSessions as unknown as SessionsClient);
  const getDbFn = (): Db => deps?.db ?? getDb();
  // The secret store defaults to a Drizzle store over the same DB. Resolved
  // lazily so importing this module does not require a DB at import time.
  const resolveSecrets = (): UserSecretStore =>
    deps?.secrets ?? makeUserSecretStore(getDbFn());
  const profiles: ProfileStore = deps?.profiles ?? makeProfileStore(getDbFn());
  const imagesClient: ImagesClient = deps?.images ?? (defaultImages as unknown as ImagesClient);
  // Lazy default (see profiles.ts): touch getDb() only when a handler reads
  // connectors, so registering without a DB doesn't throw.
  const connectors: CustomConnectorSource = deps?.connectors ?? { list: () => makeConnectorStore(getDbFn()).list() };

  router.service(TaskService, {
    // -------------------------------------------------------------------------
    // CreateTask
    // -------------------------------------------------------------------------
    async createTask(req, ctx) {
      const user = await requireUser(ctx, getSession);
      const ability = abilityFor(user);

      if (req.type !== "chat") {
        throw new ConnectError("only chat tasks exist yet", Code.InvalidArgument);
      }
      if (!ability.can("create", "Task")) {
        throw new ConnectError("forbidden", Code.PermissionDenied);
      }
      if (!req.profileId) {
        throw new ConnectError("profile_id is required", Code.InvalidArgument);
      }

      // 1. Load the active profile (ADR §5.1). Missing/archived → rejected.
      const profile = await profiles.getActive(req.profileId);
      if (!profile) {
        throw new ConnectError("profile not found or archived", Code.NotFound);
      }

      // 2. Resolve image_id → current image_uri (ADR §5.2). Defense in depth
      //    behind the DisableImage guard (Task 8): reject if no longer enabled.
      const catalog = await imagesClient.listEnabledImages({});
      const image = catalog.images.find((i) => i.id === profile.imageId);
      if (!image) {
        throw new ConnectError(
          "the profile's image is no longer enabled — contact an admin",
          Code.FailedPrecondition,
        );
      }

      // 3. Assemble harness_env (ADR §5.4), lowest → highest precedence:
      //    user Claude token (only if include_user_tokens) < profile env_vars.
      //    NEVER log values.
      const harness: Record<string, string> = {};
      if (profile.includeUserTokens) {
        try {
          const userToken = await resolveSecrets().get(user.id, CLAUDE_OAUTH_ENV_VAR);
          if (userToken) harness[CLAUDE_OAUTH_ENV_VAR] = userToken;
        } catch (secretErr) {
          console.warn(
            `[TaskService] createTask: token lookup failed for user ${user.id} — booting without it`,
            secretErr,
          );
        }
      }
      // ADR 0058: CLI integrations. Resolve the registry once (reused by the
      // policy compile below), then merge each enabled CLI's dummy env (so the
      // tool stops gating on local auth — the real credential is injected
      // host-side by the egress proxy, never in the guest) + the enabled-CLI
      // catalog the in-guest `engrams integrations` helper reads. Profile
      // envVars still override (applied last).
      const registry = await loadRegistry(connectors);
      const cliPlan = compileCliIntegrations(profile.capabilities, registry);
      for (const [k, v] of Object.entries(cliPlan.dummyEnv)) harness[k] = v;
      if (cliPlan.enabled.length > 0) harness.ENGRAM_CLI_INTEGRATIONS = JSON.stringify(cliPlan.enabled);
      // ADR 0058 P3: enabled remote MCP servers ride one more harness_env var (no
      // CreateSessionRequest change); the harness writes the `--mcp-config` file
      // from it. Auth/egress are already compiled into the policy below (inject +
      // #420 hosts-union), so this carries only { name, url } — never a token.
      const mcpPlan = compileMcpIntegrations(profile.capabilities, registry);
      if (mcpPlan.enabled.length > 0) harness.ENGRAM_MCP_INTEGRATIONS = JSON.stringify(mcpPlan.enabled);
      for (const [k, v] of Object.entries(profile.envVars)) harness[k] = v; // profile overrides
      const harnessEnv = Object.keys(harness).length > 0 ? harness : undefined;

      // 4. Create the upstream session. Mode is always "agent" (ADR §5). The
      //    profile's selected skills (ADR 0055) ride as bundle names; the
      //    coordinator resolves them to reserved-slot mounts at boot. The
      //    profile's capabilities (ADR 0056) ride as "provider:action[@resource]"
      //    strings; the coordinator binds them to the session (+ later clamps).
      //    The capabilities are also compiled here (B′) against the connector
      //    config into a per-session IntegrationPolicy — the activated Plane-B
      //    injects + response-observation specs — which rides as a JSON string
      //    the coordinator persists, resolving inject secret_refs host-side and
      //    shipping the observes to the proxy. Only shipped when non-empty (a
      //    capability-less profile, or one whose ops declare no inject/asset).
      // ADR 0057: the policy is now the full session policy — it also carries
      // the profile's network allow-list + injected secrets, which the
      // coordinator sources the egress policy from. Shipped whenever it carries
      // anything (caps OR secrets OR a non-trivial network).
      const policy = compileIntegrationPolicy(profile.capabilities, registry, {
        network: profile.network,
        secrets: profile.secrets,
      });
      const integrationPolicyJson = policyHasContent(policy)
        ? JSON.stringify(policy)
        : undefined;
      // ADR 0058: enabling a bundled CLI integration adds the shared
      // integrations-cli bundle to the session's selected skills (one dyn_* slot
      // for all CLIs), unioned with the profile's own skills.
      const selectedSkills = [...new Set([...profile.skills, ...cliPlan.bundles])];
      const created = await sessionsClient.createSession({
        imageUri: image.imageUri,
        mode: "agent",
        ...(req.prompt != null ? { prompt: req.prompt } : {}),
        ...(harnessEnv != null ? { harnessEnv } : {}),
        ...(selectedSkills.length > 0 ? { selectedSkills } : {}),
        ...(profile.capabilities.length > 0 ? { capabilities: profile.capabilities } : {}),
        ...(integrationPolicyJson != null ? { integrationPolicyJson } : {}),
      });

      // 5. Insert task + task_session (recording profile_id). Compensate on failure.
      const taskId = crypto.randomUUID();
      try {
        const db = getDbFn();
        await db.transaction(async (tx) => {
          await tx.insert(taskTable).values({
            id: taskId,
            type: "chat",
            title: req.title ?? null,
            status: "open",
            createdByUserId: user.id,
            source: {},
          });
          await tx.insert(taskSessionTable).values({
            taskId,
            sessionId: created.sessionId,
            role: "primary",
            profileId: profile.id,
          });
        });
      } catch (dbErr) {
        try {
          await sessionsClient.deleteSession({ sessionId: created.sessionId });
        } catch (delErr) {
          console.error(
            `[TaskService] createTask compensation: failed to delete orphan session ${created.sessionId} after DB error`,
            delErr,
          );
        }
        throw dbErr;
      }

      evictOwnerCacheEntry(created.sessionId);
      const loaded = await loadTask(taskId, getDbFn(), sessionsClient, profiles, imagesClient);
      return { task: loaded };
    },

    // -------------------------------------------------------------------------
    // ListTasks
    // -------------------------------------------------------------------------
    async listTasks(_req, ctx) {
      const user = await requireUser(ctx, getSession);
      const ability = abilityFor(user);
      const isAdmin = user.role === "admin";
      const db = getDbFn();

      // Fetch all task rows + their session refs in one shot.
      const taskRows = await db.select().from(taskTable);
      const sessionRefRows = await db.select().from(taskSessionTable);

      // Group session refs by taskId.
      const refsByTaskId = new Map<
        string,
        Array<{ sessionId: string; role: string | null; profileId: string | null }>
      >();
      for (const ref of sessionRefRows) {
        const existing = refsByTaskId.get(ref.taskId) ?? [];
        existing.push({ sessionId: ref.sessionId, role: ref.role, profileId: ref.profileId });
        refsByTaskId.set(ref.taskId, existing);
      }

      // Resolve profile snapshots once for all refs (one images call + one profile query).
      const profileMap = await buildProfileMap(sessionRefRows, profiles, imagesClient);

      // Collect all known sessionIds from task_session table.
      const knownSessionIds = new Set(sessionRefRows.map((r) => r.sessionId));

      // Fetch all live sessions from control plane once.
      let allSessions: Session[] = [];
      try {
        const resp = await sessionsClient.listSessions({});
        allSessions = resp.sessions
          .map((item) => item.session)
          .filter((s): s is Session => s != null);
      } catch {
        // Upstream unavailable — serve with no live session state.
        allSessions = [];
      }

      // Build a map of sessionId → Session for join lookups.
      const sessionMap = new Map<string, Session>();
      for (const sess of allSessions) {
        sessionMap.set(sess.id, sess);
      }

      // Filter task rows by ability (member sees own; admin sees all).
      const visibleTasks: Task[] = [];
      for (const row of taskRows) {
        if (
          !ability.can(
            "read",
            subject("Task", { createdByUserId: row.createdByUserId }),
          )
        ) {
          continue;
        }
        const refs = refsByTaskId.get(row.id) ?? [];
        visibleTasks.push(buildTask(row, refs, sessionMap, profileMap));
      }

      // For admins: surface unattributed sessions as synthetic rows.
      if (isAdmin) {
        for (const sess of allSessions) {
          if (!knownSessionIds.has(sess.id)) {
            visibleTasks.push(buildUnattributedTask(sess));
          }
        }
      }

      return { tasks: visibleTasks };
    },

    // -------------------------------------------------------------------------
    // GetTask
    // -------------------------------------------------------------------------
    async getTask(req, ctx) {
      const user = await requireUser(ctx, getSession);
      const ability = abilityFor(user);
      const db = getDbFn();

      // Fetch task row.
      const taskRows = await db
        .select()
        .from(taskTable)
        .where(eq(taskTable.id, req.taskId))
        .limit(1);

      if (taskRows.length === 0) {
        // Not found — return NotFound (no enumeration leak).
        throw new ConnectError("not found", Code.NotFound);
      }
      const taskRow = taskRows[0]!;

      // Anti-enumeration: task exists but is owned by someone else → NotFound.
      if (
        !ability.can(
          "read",
          subject("Task", { createdByUserId: taskRow.createdByUserId }),
        )
      ) {
        throw new ConnectError("not found", Code.NotFound);
      }

      const loaded = await loadTask(req.taskId, db, sessionsClient, profiles, imagesClient);
      return { task: loaded };
    },

    // -------------------------------------------------------------------------
    // DeleteTask
    // -------------------------------------------------------------------------
    async deleteTask(req, ctx) {
      const user = await requireUser(ctx, getSession);
      const ability = abilityFor(user);
      const db = getDbFn();

      // Fetch task row.
      const taskRows = await db
        .select()
        .from(taskTable)
        .where(eq(taskTable.id, req.taskId))
        .limit(1);

      if (taskRows.length === 0) {
        // Not found — return NotFound (no enumeration leak).
        throw new ConnectError("not found", Code.NotFound);
      }
      const taskRow = taskRows[0]!;

      // Anti-enumeration: task exists but is owned by someone else → NotFound.
      if (
        !ability.can(
          "delete",
          subject("Task", { createdByUserId: taskRow.createdByUserId }),
        )
      ) {
        throw new ConnectError("not found", Code.NotFound);
      }

      // Fetch task_session rows to know which upstream sessions to delete.
      const sessionRefRows = await db
        .select()
        .from(taskSessionTable)
        .where(eq(taskSessionTable.taskId, req.taskId));

      // Delete each upstream session. Best-effort: log failures but don't abort.
      for (const ref of sessionRefRows) {
        try {
          await sessionsClient.deleteSession({ sessionId: ref.sessionId });
        } catch (err) {
          console.error(
            `[TaskService] deleteTask: failed to delete upstream session ${ref.sessionId}`,
            err,
          );
        }
      }

      // Delete the task row (cascade deletes task_session rows via FK).
      await db.delete(taskTable).where(eq(taskTable.id, req.taskId));

      return {};
    },
  });
}
