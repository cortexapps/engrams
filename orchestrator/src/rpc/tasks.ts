/**
 * Native TaskService implementation (ADR 0039 §3, Task 19).
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
import {
  sessions as defaultSessions,
  secrets as defaultSecrets,
} from "../control-plane/client.ts";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Subset of SessionService client used by TaskService. */
export interface SessionsClient {
  createSession(req: {
    imageUri: string;
    mode: string;
    prompt?: string;
    harnessSecretId?: string;
  }): Promise<{ sessionId: string; status: string; imageVersion: string; kind: string }>;
  listSessions(req: Record<string, never>): Promise<{ sessions: Array<{ session?: Session | undefined }> }>;
  getSession(req: { sessionId: string }): Promise<{ session?: Session | undefined }>;
  deleteSession(req: { sessionId: string }): Promise<unknown>;
}

/** Subset of SecretService client used by TaskService. */
export interface SecretsClient {
  hasSecret(req: { key: string }): Promise<{ exists: boolean }>;
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
  secrets?: SecretsClient;
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
  sessionRefs: Array<{ sessionId: string; role: string | null }>,
  sessionMap: Map<string, Session>,
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
    return {
      sessionId: ref.sessionId,
      ...(ref.role != null ? { role: ref.role } : {}),
      ...(liveSession != null ? { session: liveSession } : {}),
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

  return buildTask(taskRow, sessionRefRows, sessionMap);
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
  const secretsClient: SecretsClient = deps?.secrets ?? (defaultSecrets as unknown as SecretsClient);
  const getDbFn = (): Db => deps?.db ?? getDb();

  router.service(TaskService, {
    // -------------------------------------------------------------------------
    // CreateTask
    // -------------------------------------------------------------------------
    async createTask(req, ctx) {
      const user = await requireUser(ctx, getSession);
      const ability = abilityFor(user);

      // Only "chat" tasks exist yet.
      if (req.type !== "chat") {
        throw new ConnectError("only chat tasks exist yet", Code.InvalidArgument);
      }

      if (!ability.can("create", "Task")) {
        throw new ConnectError("forbidden", Code.PermissionDenied);
      }

      // Check for harness token. hasSecret returns {exists: false} for everyone
      // until Task 20 (routes/me.ts) writes it — no-harness images work fine.
      let harnessSecretId: string | undefined;
      try {
        const { exists } = await secretsClient.hasSecret({ key: user.id });
        harnessSecretId = exists ? user.id : undefined;
      } catch (secretErr) {
        // hasSecret failure is non-fatal — proceed without harness token.
        console.warn(
          `[TaskService] createTask: hasSecret check failed for user ${user.id} — booting without harness token`,
          secretErr,
        );
        harnessSecretId = undefined;
      }

      // 1. Create the upstream session (control plane).
      const created = await sessionsClient.createSession({
        imageUri: req.imageUri,
        mode: "agent",
        ...(req.prompt != null ? { prompt: req.prompt } : {}),
        ...(harnessSecretId != null ? { harnessSecretId } : {}),
      });

      // 2. Insert task + task_session rows. Compensate on failure.
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
          });
        });
      } catch (dbErr) {
        // Compensation: upstream session created but DB insert failed.
        // Best-effort delete the session to avoid orphans.
        try {
          await sessionsClient.deleteSession({ sessionId: created.sessionId });
        } catch (delErr) {
          // Log compensation failure but don't mask the original error.
          console.error(
            `[TaskService] createTask compensation: failed to delete orphan session ${created.sessionId} after DB error`,
            delErr,
          );
        }
        throw dbErr;
      }

      // 3. Evict the negative-cache entry so authz/resolve.ts returns the new
      //    owner immediately (avoids ≤5 s stale null window).
      evictOwnerCacheEntry(created.sessionId);

      // 4. Return the newly created task with live session state.
      const loaded = await loadTask(taskId, getDbFn(), sessionsClient);
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
      const refsByTaskId = new Map<string, Array<{ sessionId: string; role: string | null }>>();
      for (const ref of sessionRefRows) {
        const existing = refsByTaskId.get(ref.taskId) ?? [];
        existing.push({ sessionId: ref.sessionId, role: ref.role });
        refsByTaskId.set(ref.taskId, existing);
      }

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
        visibleTasks.push(buildTask(row, refs, sessionMap));
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

      const loaded = await loadTask(req.taskId, db, sessionsClient);
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
