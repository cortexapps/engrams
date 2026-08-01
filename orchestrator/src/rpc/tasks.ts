/**
 * Native TaskService implementation (ADR 0051 §3, Task 19).
 *
 * This service is orchestrator-native — it is registered on the same
 * ConnectRouter as the generic passthrough, but is NEVER proxied to the
 * control plane. The web client sees one uniform generated API.
 *
 * Session status mapping (control-plane → task status):
 *   pending | created | active | idle | evacuating | evicting
 *     → "working"   (session is alive / in progress)
 *   completed
 *     → "done"      (session ran to completion)
 *   failed | dead | host_lost
 *     → "failed"    (session is unrecoverable)
 *   (unrecognised / blank)
 *     → keep task's own persisted status ("open" at create, unchanged)
 *
 * List semantics (ADR 0087):
 *   Scope, owner, and persisted-task free-text search filters run in SQL; live
 *   display-state filtering, recent-activity ordering, and pagination run after
 *   the control-plane join. Synthetic unattributed rows are the sole search
 *   exception because they have no DB representation. Filters narrow inside
 *   the caller's ability; they never widen it.
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
import { and, eq, exists, ilike, inArray, isNull, or, type SQL } from "drizzle-orm";

import { TaskService } from "../gen/engram/app/v1/task_pb.ts";
import type { Task, TaskSessionRef } from "../gen/engram/app/v1/task_pb.ts";
import type { Session } from "../gen/engram/app/v1/session_pb.ts";

import { log as rootLog } from "../log.ts";
import { abilityFor } from "../authz/ability.ts";
import { getSessionFromHeaders } from "../auth/session.ts";
import { isServiceAccountEmail } from "./api-key.ts";
import { getDb } from "../db/client.ts";
import { task as taskTable, taskSession as taskSessionTable } from "../db/schema.ts";
import {
  sessions as defaultSessions,
  images as defaultImages,
  harnessCatalog as defaultHarnessCatalog,
} from "../control-plane/client.ts";

const log = rootLog.child({ component: "task" });
import { makeUserSecretStore, type UserSecretStore } from "../db/user-secrets.ts";
import { makeProfileStore, type ProfileStore } from "../db/profiles.ts";
import { makePortExposureStore, type PortExposureStore } from "../db/port-exposures.ts";
import {
  makeUserIdentityStore,
  type UserIdentity,
  type UserIdentityStore,
} from "../db/users.ts";
import type { ImagesClient } from "./profiles.ts";
import type { CustomConnectorSource } from "../connectors/registry.ts";
import {
  makePendingToolCallStore,
  type PendingToolCallStore,
} from "../tools/pending-tool-calls.ts";
import {
  createTaskWithSession,
  type Db,
  type HarnessCatalogClient,
} from "./task-create.ts";
import { makeConnectorStore } from "../db/connectors.ts";
import type { IntegrationConnectionStore } from "../db/integration-connections.ts";

// Re-export ImagesClient so downstream modules (image-guard, tests) can import
// it from tasks.ts. The canonical declaration lives in rpc/profiles.ts.
export type { ImagesClient } from "./profiles.ts";
// Re-export the Drizzle DB handle so existing importers (tests) keep their
// `import type { Db } from "../rpc/tasks.ts"`. Canonical home is task-create.ts.
export type { Db } from "./task-create.ts";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Subset of SessionService client used by TaskService. */
export interface SessionsClient {
  createSession(req: {
    requestedSessionId?: string;
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
    // ADR 0062/0063: the selected harness (catalog name) the coordinator mounts
    // on dyn_0 + execs (proto CreateSessionRequest.harness). Resolved from
    // session override ?? profile ?? deployment default.
    harness?: string;
    // ADR 0107: session mode for the initial prompt (e.g. "plan"); the
    // coordinator validates it against the harness descriptor's modes.
    harnessMode?: string;
  }): Promise<{ sessionId: string; status: string; imageVersion: string; kind: string }>;
  listSessions(req: Record<string, never>): Promise<{ sessions: Array<{ session?: Session | undefined }> }>;
  getSession(req: { sessionId: string }): Promise<{ session?: Session | undefined }>;
  deleteSession(req: { sessionId: string }): Promise<unknown>;
}

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
  harnessCatalog?: HarnessCatalogClient;
  /** Per-user KEK-sealed session secret store (ADR 0051 Drip A). */
  secrets?: UserSecretStore;
  /** Admin-curated session profiles (ADR 0053). */
  profiles?: ProfileStore;
  /** Enabled-image catalog client (ADR 0053) — resolves image_id → image_uri. */
  images?: ImagesClient;
  /** Connector catalog (ADR 0057) — custom connectors merged with built-in seeds. */
  connectors?: CustomConnectorSource;
  /** Port-exposure store (ADR 0064) — auto-mints profile.portExposures at create. */
  portExposures?: PortExposureStore;
  /** Owner identity lookup for git attribution and task read enrichment. */
  users?: UserIdentityStore;
  connections?: IntegrationConnectionStore;
  /** Register a session for stream-listener scanner discovery. */
  db?: Db;
  /** ADR 0107: the pending-tool-call ledger, for the awaiting_review derivation. */
  pendingCalls?: PendingToolCallStore;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Escape a literal substring for use as a PostgreSQL LIKE/ILIKE pattern. */
export function searchPattern(q: string): string {
  return `%${q.replace(/[\\%_]/g, "\\$&")}%`;
}

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
): Promise<{ id: string; role: string; serviceAccount: boolean }> {
  const session = await getSession(headersOf(ctx));
  if (!session) {
    throw new ConnectError("unauthenticated", Code.Unauthenticated);
  }
  return {
    id: session.user.id,
    role: session.user.role ?? "user",
    // A global API key resolves to its service-account owner (ADR 0086) —
    // a PROGRAMMATIC principal: it has no per-user harness token, so task
    // compilation must take the org-credential path even for "chat" tasks
    // (the CI create-session smoke failed "not logged in" without this).
    serviceAccount: isServiceAccountEmail(session.user.email ?? ""),
  };
}

/**
 * Map a control-plane session status string to a task status string.
 *
 * Session status ∈ { pending, queued, created, active, unreachable, parked,
 *                    idle, evacuating, evicting, completed, failed, dead,
 *                    host_lost }
 *
 * Mapping (documented in module JSDoc above):
 *   pending | queued | created | active | unreachable | parked | idle |
 *     evacuating | evicting → working
 *   completed → done
 *   failed | dead | host_lost → failed
 *   (anything else) → null (caller keeps persisted task status)
 */
function sessionStatusToTaskStatus(sessionStatus: string): string | null {
  switch (sessionStatus) {
    case "pending":
    // Queued = waiting FIFO for capacity (ADR 0048) — in progress, not open.
    case "queued":
    case "created":
    case "active":
    // Unreachable = mid-recovery (ADR 0091) — the next prompt/resume
    // restores from checkpoint; the task is still being worked.
    case "unreachable":
    case "parked":
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

/** Max length (code points) of a user-set sticky custom title. */
const CUSTOM_TITLE_MAX_CHARS = 200;

/** The task-title columns the effective-title derivation reads. */
export interface TitleRow {
  title: string | null;
  suggestedTitle: string | null;
  customTitle: string | null;
}

/**
 * The effective display title, in precedence order:
 *   1. `custom_title` — the user's sticky rename (harness suggestions never
 *      override it);
 *   2. the freshest harness suggestion — the LIVE session value if the session
 *      is still up, else the persisted `suggested_title` snapshot (so an old
 *      chat keeps its nice title after its session is GC'd);
 *   3. `title` — the truncated-prompt default.
 * `null` when nothing is set (a brand-new task with no prompt).
 */
export function effectiveTitle(
  row: TitleRow,
  liveSuggested: string | null | undefined,
): string | null {
  return row.customTitle ?? liveSuggested ?? row.suggestedTitle ?? row.title ?? null;
}

/**
 * Build a proto Task from a DB row + a map of sessionId → live Session.
 * The sessionMap may be empty (no upstream session found → session field unset).
 * The identityMap is a best-effort directory join keyed by createdByUserId.
 *
 * `snapshots` (optional): the opportunistic-snapshot sink. When the primary
 * session's LIVE `suggested_title` differs from the persisted snapshot, we push
 * `{taskId, suggestedTitle}` so the caller can fire-and-forget the write —
 * keeping the snapshot fresh (and durable past session GC) off the read path
 * the orchestrator already performs. Only pushed on an actual change, so steady
 * state produces no writes.
 */
function buildTask(
  row: {
    id: string;
    type: string;
    title: string | null;
    suggestedTitle: string | null;
    customTitle: string | null;
    status: string;
    createdByUserId: string | null;
    source: unknown;
    createdAt: Date;
  },
  sessionRefs: Array<{ sessionId: string; role: string | null; profileId: string | null }>,
  sessionMap: Map<string, Session>,
  profileMap: Map<string, { id: string; name: string; icon: string; archived: boolean; imageUri: string; skills: string[] }>,
  identityMap: Map<string, UserIdentity>,
  snapshots?: Array<{ taskId: string; suggestedTitle: string }>,
  awaitingSessionIds?: ReadonlySet<string>,
): Task {
  // Derive status + the live harness title from the primary session (if available).
  let status = row.status;
  const primaryRef = sessionRefs.find((r) => r.role === "primary") ?? sessionRefs[0];
  const primarySession = primaryRef ? sessionMap.get(primaryRef.sessionId) : undefined;
  if (primarySession) {
    const mapped = sessionStatusToTaskStatus(primarySession.status);
    if (mapped !== null) status = mapped;
    // ADR 0107: a live session parked on an unsubmitted session-handled tool
    // call (a plan awaiting review, an unanswered question) is WAITING ON THE
    // USER — the contracted `awaiting_review` status, derived at read time
    // like the rest (the row status stays untouched).
    if (mapped === "working" && awaitingSessionIds?.has(primaryRef!.sessionId)) {
      status = "awaiting_review";
    }
  }
  const liveSuggested = primarySession?.suggestedTitle;

  // Opportunistic snapshot: the coordinator holds the freshest harness title on
  // the live session; mirror it onto the task row (only when it actually
  // changed) so it survives the session's eventual GC.
  if (
    snapshots != null &&
    liveSuggested != null &&
    liveSuggested !== "" &&
    liveSuggested !== row.suggestedTitle
  ) {
    snapshots.push({ taskId: row.id, suggestedTitle: liveSuggested });
  }

  const title = effectiveTitle(row, liveSuggested);
  const creator = row.createdByUserId != null
    ? identityMap.get(row.createdByUserId)
    : undefined;

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
    ...(title != null ? { title } : {}),
    titleIsCustom: row.customTitle != null,
    status,
    ...(row.createdByUserId != null ? { createdByUserId: row.createdByUserId } : {}),
    ...(row.createdByUserId != null && creator != null
      ? {
          createdBy: {
            id: row.createdByUserId,
            name: creator.name,
            email: creator.email,
          },
        }
      : {}),
    sourceJson: JSON.stringify(row.source ?? {}),
    sessions,
    createdAt: row.createdAt.toISOString(),
  } as Task;
}

/**
 * Fire-and-forget the opportunistic `suggested_title` snapshot writes buildTask
 * collected. Best-effort: a failure only means the display title lags the live
 * session; never blocks or fails the read.
 */
function persistTitleSnapshots(
  db: Db,
  updates: Array<{ taskId: string; suggestedTitle: string }>,
): void {
  if (updates.length === 0) return;
  void Promise.all(
    updates.map((u) =>
      db.update(taskTable).set({ suggestedTitle: u.suggestedTitle }).where(eq(taskTable.id, u.taskId)),
    ),
  ).catch((err) => {
    log.warn({ err }, "task-title snapshot write failed (continuing)");
  });
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
/** ADR 0107: best-effort fetch of the sessions waiting on the user — the
 *  attention affordance must never break the task list. */
async function fetchAwaitingSessionIds(
  pendingCalls: PendingToolCallStore,
): Promise<ReadonlySet<string>> {
  try {
    return new Set(await pendingCalls.listSessionIdsWithPendingSessionCalls());
  } catch {
    return new Set();
  }
}

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
): Promise<Map<string, { id: string; name: string; icon: string; archived: boolean; imageUri: string; skills: string[] }>> {
  const ids = [...new Set(refs.map((r) => r.profileId).filter((x): x is string => x != null))];
  const out = new Map<string, { id: string; name: string; icon: string; archived: boolean; imageUri: string; skills: string[] }>();
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
      // ADR 0064: carry the profile's skills so the web can gate optional
      // session capabilities (the BROWSER tab) off the snapshot it already
      // fetches — no separate capability call.
      skills: p.skills,
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
  users: UserIdentityStore,
  pendingCalls: PendingToolCallStore = makePendingToolCallStore(),
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

  const [profileMap, identityMap, awaitingSessionIds] = await Promise.all([
    buildProfileMap(sessionRefRows, profiles, imagesClient),
    users.getIdentities(taskRow.createdByUserId != null ? [taskRow.createdByUserId] : []),
    fetchAwaitingSessionIds(pendingCalls),
  ]);
  const snapshots: Array<{ taskId: string; suggestedTitle: string }> = [];
  const task = buildTask(
    taskRow,
    sessionRefRows,
    sessionMap,
    profileMap,
    identityMap,
    snapshots,
    awaitingSessionIds,
  );
  persistTitleSnapshots(db_, snapshots);
  return task;
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
    getSessionFromHeaders;

  const sessionsClient: SessionsClient = deps?.sessions ?? (defaultSessions as unknown as SessionsClient);
  const getDbFn = (): Db => deps?.db ?? getDb();
  // The secret store defaults to a Drizzle store over the same DB. Resolved
  // lazily so importing this module does not require a DB at import time.
  const resolveSecrets = (): UserSecretStore =>
    deps?.secrets ?? makeUserSecretStore(getDbFn());
  const profiles: ProfileStore = deps?.profiles ?? makeProfileStore(getDbFn());
  // Lazy (like resolveSecrets): touch getDb() only when createTask actually runs,
  // so registering without a DB (the auth/validation tests) doesn't throw.
  const resolvePortExposures = (): PortExposureStore =>
    deps?.portExposures ?? makePortExposureStore(getDbFn());
  // Lazy like the other DB-backed stores: registration itself must not require
  // a configured database.
  const resolveUsers = (): UserIdentityStore =>
    deps?.users ?? makeUserIdentityStore(getDbFn());
  const imagesClient: ImagesClient = deps?.images ?? (defaultImages as unknown as ImagesClient);
  // Lazy like the other DB-backed stores (ADR 0107 attention derivation).
  let pendingCallsStoreMemo: PendingToolCallStore | undefined;
  const pendingCallsStore = {
    listSessionIdsWithPendingSessionCalls: () => {
      pendingCallsStoreMemo ??= deps?.pendingCalls ?? makePendingToolCallStore();
      return pendingCallsStoreMemo.listSessionIdsWithPendingSessionCalls();
    },
  } as PendingToolCallStore;
  const harnessCatalogClient: HarnessCatalogClient =
    deps?.harnessCatalog ?? (defaultHarnessCatalog as unknown as HarnessCatalogClient);
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

      // Create the task + its primary session via the shared primitive — the
      // SAME path the external-trigger ThreadControlPlane uses (ADR 0060), so a
      // UI chat task and a triggered session share one privilege/compensation
      // path. Throws NotFound (profile missing/archived) or FailedPrecondition
      // (image disabled); compensates the orphan session on a DB failure.
      const { taskId } = await createTaskWithSession(
        {
          profiles,
          images: imagesClient,
          connectors,
          harnessCatalog: harnessCatalogClient,
          sessions: sessionsClient,
          secrets: resolveSecrets(),
          portExposures: resolvePortExposures(),
          users: resolveUsers(),
          ...(deps?.connections ? { connections: deps.connections } : {}),
          db: getDbFn(),
        },
        {
          type: "chat",
          ownerUserId: user.id,
          // A service-account creator (API key, e.g. CI) has no per-user
          // harness token — compile the org-credential path (ADR 0063 B4)
          // even though the task type is "chat".
          ownerIsServiceAccount: user.serviceAccount,
          profileId: req.profileId,
          title: req.title ?? null,
          ...(req.prompt != null ? { prompt: req.prompt } : {}),
          ...(req.harness != null ? { harness: req.harness } : {}),
          ...(req.model != null ? { model: req.model } : {}),
          ...(req.effort != null ? { effort: req.effort } : {}),
          ...(req.harnessMode != null ? { harnessMode: req.harnessMode } : {}),
        },
      );

      const loaded = await loadTask(
        taskId,
        getDbFn(),
        sessionsClient,
        profiles,
        imagesClient,
        resolveUsers(),
        pendingCallsStore,
      );
      return { task: loaded };
    },

    // -------------------------------------------------------------------------
    // ListTasks
    // -------------------------------------------------------------------------
    async listTasks(req, ctx) {
      const user = await requireUser(ctx, getSession);
      const ability = abilityFor(user);
      const isAdmin = user.role === "admin";
      const db = getDbFn();

      if (req.scope !== "" && req.scope !== "mine" && req.scope !== "all") {
        throw new ConnectError("invalid scope", Code.InvalidArgument);
      }
      if (req.scope === "all" && !isAdmin) {
        throw new ConnectError("forbidden", Code.PermissionDenied);
      }

      const mineOnly = req.scope === "mine" || (req.scope === "" && !isAdmin);
      const q = req.search.trim();
      const taskConditions: SQL[] = [];
      if (mineOnly) {
        taskConditions.push(eq(taskTable.createdByUserId, user.id));
      }
      const owners = req.createdByUserIds;
      if (owners.length > 0) {
        const ownerConds: SQL[] = [];
        const ownerIds = owners.filter((owner) => owner !== "system");
        if (ownerIds.length > 0) {
          ownerConds.push(inArray(taskTable.createdByUserId, ownerIds));
        }
        if (owners.includes("system")) {
          ownerConds.push(isNull(taskTable.createdByUserId));
        }
        const ownerCondition = or(...ownerConds);
        if (ownerCondition) {
          taskConditions.push(ownerCondition);
        }
      }
      if (q !== "") {
        const pattern = searchPattern(q);
        // Title matching covers every PERSISTED tier of the display title —
        // custom rename, persisted harness suggestion, and the truncated-prompt
        // default. Only a LIVE suggestion not yet echoed to suggested_title
        // (effectiveTitle joins it from the session) is invisible to this SQL;
        // searching that would mean re-running search post-join. Deferred —
        // the echo closes the gap at the next persist.
        const searchCondition = or(
          ilike(taskTable.customTitle, pattern),
          ilike(taskTable.suggestedTitle, pattern),
          ilike(taskTable.title, pattern),
          ilike(taskTable.id, pattern),
          exists(
            db
              .select({ taskId: taskSessionTable.taskId })
              .from(taskSessionTable)
              .where(
                and(
                  eq(taskSessionTable.taskId, taskTable.id),
                  ilike(taskSessionTable.sessionId, pattern),
                ),
              ),
          ),
        );
        if (searchCondition) {
          taskConditions.push(searchCondition);
        }
      }

      const taskQuery = db.select().from(taskTable);
      const taskRows = taskConditions.length > 0
        ? await taskQuery.where(and(...taskConditions))
        : await taskQuery;
      const creatorIds = [
        ...new Set(
          taskRows
            .map((row) => row.createdByUserId)
            .filter((id): id is string => id != null),
        ),
      ];
      const identityMap = await resolveUsers().getIdentities(creatorIds);

      const taskIds = taskRows.map((row) => row.id);
      const sessionRefRows = mineOnly
        ? taskIds.length > 0
          ? await db
              .select()
              .from(taskSessionTable)
              .where(inArray(taskSessionTable.taskId, taskIds))
          : []
        : await db.select().from(taskSessionTable);

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

      // ADR 0107: sessions parked on an unsubmitted session-handled call.
      const awaitingSessionIds = await fetchAwaitingSessionIds(pendingCallsStore);

      // Filter task rows by ability (belt-and-braces under SQL scoping).
      const visibleTasks: Task[] = [];
      const snapshots: Array<{ taskId: string; suggestedTitle: string }> = [];
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
        visibleTasks.push(
          buildTask(row, refs, sessionMap, profileMap, identityMap, snapshots, awaitingSessionIds),
        );
      }
      // Fire-and-forget the freshest harness titles onto the task rows (only
      // the ones that changed) so the list keeps them past session GC.
      persistTitleSnapshots(db, snapshots);

      // Fleet-wide views surface unattributed sessions unless the owner filter
      // excludes system-owned rows. These synthetic rows have no DB
      // representation, so they are the sole exception to SQL-backed search.
      if (!mineOnly && (owners.length === 0 || owners.includes("system"))) {
        const syntheticSearch = q.toLowerCase();
        for (const sess of allSessions) {
          if (knownSessionIds.has(sess.id)) continue;
          if (
            syntheticSearch !== "" &&
            !`unattributed-${sess.id}`.toLowerCase().includes(syntheticSearch) &&
            !sess.id.toLowerCase().includes(syntheticSearch)
          ) continue;
          visibleTasks.push(buildUnattributedTask(sess));
        }
      }

      let filteredTasks = visibleTasks;

      if (req.states.length > 0) {
        const states = new Set(req.states);
        filteredTasks = filteredTasks.filter((task) => {
          const displayState = task.sessions[0]?.session?.status ?? "pending";
          return states.has(displayState);
        });
      }

      filteredTasks.sort((a, b) => {
        const aActiveAt = a.sessions[0]?.session?.lastActiveAt ?? a.createdAt;
        const bActiveAt = b.sessions[0]?.session?.lastActiveAt ?? b.createdAt;
        return Date.parse(bActiveAt) - Date.parse(aActiveAt);
      });

      const totalCount = filteredTasks.length;
      if (req.pageSize <= 0) {
        return { tasks: filteredTasks, totalCount };
      }

      const pageSize = Math.min(req.pageSize, 1000);
      const page = Math.max(1, req.page || 1);
      const offset = (page - 1) * pageSize;
      return {
        tasks: filteredTasks.slice(offset, offset + pageSize),
        totalCount,
      };
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

      const loaded = await loadTask(
        req.taskId,
        db,
        sessionsClient,
        profiles,
        imagesClient,
        resolveUsers(),
        pendingCallsStore,
      );
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

    // -------------------------------------------------------------------------
    // UpdateTask — rename (present title) or reset-to-auto (absent title).
    // -------------------------------------------------------------------------
    async updateTask(req, ctx) {
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

      // Owner or admin only. Anti-enumeration: a task owned by someone else
      // returns NotFound, not PermissionDenied. Renaming is a `manage` verb —
      // granted on owned Tasks and, via `manage:all`, to admins (the `Actions`
      // union has no distinct "update"; `manage` is the capability owners hold).
      if (
        !ability.can(
          "manage",
          subject("Task", { createdByUserId: taskRow.createdByUserId }),
        )
      ) {
        throw new ConnectError("not found", Code.NotFound);
      }

      // Present `title` ⇒ set the STICKY custom title (harness suggestions no
      // longer override it). Absent ⇒ clear it (reset), so the effective title
      // falls back to the freshest harness suggestion, then the truncated
      // prompt. A blank/whitespace title is rejected — clearing is done by
      // omitting the field, never by sending "".
      let customTitle: string | null;
      if (req.title !== undefined) {
        const trimmed = req.title.trim();
        if (trimmed === "") {
          throw new ConnectError(
            "title must not be blank — omit it to reset to the auto title",
            Code.InvalidArgument,
          );
        }
        if ([...trimmed].length > CUSTOM_TITLE_MAX_CHARS) {
          throw new ConnectError(
            `title must be at most ${CUSTOM_TITLE_MAX_CHARS} characters`,
            Code.InvalidArgument,
          );
        }
        customTitle = trimmed;
      } else {
        customTitle = null;
      }

      // `task.updatedAt` has no `$onUpdate`, so bump it explicitly.
      await db
        .update(taskTable)
        .set({ customTitle, updatedAt: new Date() })
        .where(eq(taskTable.id, req.taskId));

      const loaded = await loadTask(
        req.taskId,
        db,
        sessionsClient,
        profiles,
        imagesClient,
        resolveUsers(),
        pendingCallsStore,
      );
      return { task: loaded };
    },
  });
}
