/**
 * Repo autodiscovery — the side-effecting half (pure logic: profile-repos.ts).
 *
 * Boots ONE short-lived session from the profile (the review control plane's
 * ephemeral-session pattern: a system task row + `createSessionForExistingTask`,
 * ownerless so credentials are programmatic), runs the single bounded
 * `DISCOVER_COMMAND` through durable exec, parses the checkouts + remotes, and
 * tears everything down — session and task row — whatever happens. The task is
 * system-owned (`created_by_user_id` null), so it never appears in a member's
 * task list even if teardown is interrupted.
 */

import { eq } from "drizzle-orm";

import { task as taskTable } from "../db/schema.ts";
import type { ProfileStore } from "../db/profiles.ts";
import {
  createSessionForExistingTask,
  type CreateSessionForExistingTaskDeps,
  type CreateSessionForExistingTaskParams,
  type Db,
  type TaskSessionsClient,
} from "./task-create.ts";
import {
  runExec,
  RunExecError,
  defaultRunExecRuntime,
  type DurableExecClient,
  type RunExecRuntime,
} from "../exec/durable-exec.ts";
import { DISCOVER_COMMAND, parseDiscoverOutput, type DiscoveredRepo } from "./profile-repos.ts";
import { log as rootLog } from "../log.ts";

const log = rootLog.child({ component: "profile-discover" });

/** Covers session boot (base-snapshot restore) + the bounded guest scan. */
const DISCOVER_DEADLINE_MS = 90_000;

export interface DiscoverReposDeps
  extends Omit<CreateSessionForExistingTaskDeps, "profiles" | "sessions"> {
  profiles: Pick<ProfileStore, "getActive">;
  sessions: TaskSessionsClient & DurableExecClient;
  execRuntime?: RunExecRuntime;
  /** Focused test seam; production delegates to the shared task-create helper. */
  createSession?: (
    deps: CreateSessionForExistingTaskDeps,
    params: CreateSessionForExistingTaskParams,
  ) => Promise<{ sessionId: string }>;
}

export class DiscoverReposError extends Error {}

/**
 * Discover the git checkouts inside `profileId`'s image. Throws
 * `DiscoverReposError` with an operator-actionable message on scan failure;
 * profile lookup errors surface as-is from the store.
 */
export async function discoverProfileRepos(
  deps: DiscoverReposDeps,
  profileId: string,
): Promise<DiscoveredRepo[]> {
  const profile = await deps.profiles.getActive(profileId);
  if (!profile) throw new DiscoverReposError("profile not found or archived");

  // The probe rides the task model like every session (ADR 0060) — but as a
  // SYSTEM task: no owner, so members never see it, and it is deleted below.
  const taskId = crypto.randomUUID();
  const db: Db = deps.db;
  await db.insert(taskTable).values({
    id: taskId,
    type: "profile_discover",
    title: `Repo discovery — ${profile.name}`,
    createdByUserId: null,
    source: { provider: "profile_discover", profileId },
  });

  let sessionId: string | undefined;
  try {
    const create = deps.createSession ?? createSessionForExistingTask;
    ({ sessionId } = await create(deps, {
      taskId,
      profileId,
      role: "primary",
    }));
    let result;
    try {
      result = await runExec(
        deps.sessions,
        sessionId,
        DISCOVER_COMMAND,
        { execId: `exec:${sessionId}:repo-discover`, deadlineMs: DISCOVER_DEADLINE_MS },
        deps.execRuntime ?? defaultRunExecRuntime,
      );
    } catch (err) {
      const stderr = err instanceof RunExecError ? err.stderr.trim() : "";
      throw new DiscoverReposError(
        `repo scan failed: ${err instanceof Error ? err.message : String(err)}` +
          (stderr ? ` — ${stderr}` : ""),
        { cause: err },
      );
    }
    if (result.exitStatus != null && result.exitStatus !== 0) {
      throw new DiscoverReposError(
        `repo scan exited ${result.exitStatus}: ${result.stderr.trim() || "no stderr"}`,
      );
    }
    return parseDiscoverOutput(result.stdout);
  } finally {
    // Teardown is best-effort and must never mask the scan result/error.
    if (sessionId !== undefined) {
      try {
        await deps.sessions.deleteSession({ sessionId });
      } catch (err) {
        log.warn({ sessionId, err }, "discover: session delete failed (idle eviction will reap)");
      }
    }
    try {
      // task_session rows cascade with the task row.
      await db.delete(taskTable).where(eq(taskTable.id, taskId));
    } catch (err) {
      log.warn({ taskId, err }, "discover: task cleanup failed (system task; harmless)");
    }
  }
}
