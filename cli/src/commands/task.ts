/**
 * engrams task … — the product-level verbs (native TaskService).
 *
 * A task starts from a PROFILE (ADR 0053): `task create --profile <name|id>`
 * resolves the profile server-side to an image + skills + integration grants +
 * network policy — the same compilation the web's composer runs. Sessions
 * created here are attributed to the caller (their name in the dashboard).
 */

import type { Clients } from "../client.ts";
import { detail, fail, failWith, printJson, table, truncate } from "../output.ts";
import type { Task } from "../gen/engram/app/v1/task_pb.ts";

function taskJson(t: Task) {
  return {
    id: t.id,
    type: t.type,
    title: t.title,
    status: t.status,
    created_by_user_id: t.createdByUserId,
    created_at: t.createdAt,
    sessions: t.sessions.map((s) => ({
      session_id: s.sessionId,
      role: s.role,
      status: s.session?.status,
      image: s.session?.image,
    })),
  };
}

/** Accepts a profile id or (unique) name; resolves via ListProfiles. */
async function resolveProfileId(c: Clients, ref: string): Promise<string> {
  const resp = await c.profile.listProfiles({}).catch(failWith);
  const byId = resp.profiles.find((p) => p.id === ref);
  if (byId) return byId.id;
  const byName = resp.profiles.filter((p) => p.name === ref && !p.archived);
  if (byName.length === 1) return byName[0]!.id;
  if (byName.length > 1) fail(`profile name "${ref}" is ambiguous — pass the id`);
  fail(
    `no profile "${ref}" — \`engrams profile list\` shows what's available`,
  );
}

export interface TaskCreateOpts {
  profile: string;
  prompt?: string;
  title?: string;
  harness?: string;
  model?: string;
  effort?: string;
  /** ADR 0107: session mode for the initial prompt (e.g. "plan"). */
  mode?: string;
}

export async function create(c: Clients, opts: TaskCreateOpts, json: boolean): Promise<void> {
  const profileId = await resolveProfileId(c, opts.profile);
  const resp = await c.task
    .createTask({
      type: "chat",
      profileId,
      prompt: opts.prompt,
      title: opts.title,
      harness: opts.harness,
      model: opts.model,
      effort: opts.effort,
      harnessMode: opts.mode,
    })
    .catch(failWith);
  const t = resp.task;
  if (!t) failWith(new Error("response carried no task"));
  if (json) {
    printJson(taskJson(t));
    return;
  }
  // The primary session id is what scripts want next (exec/logs/prompt).
  console.log(t.sessions[0]?.sessionId ?? t.id);
}

export async function list(c: Clients, json: boolean): Promise<void> {
  const resp = await c.task.listTasks({}).catch(failWith);
  if (json) {
    printJson({ tasks: resp.tasks.map(taskJson) });
    return;
  }
  if (resp.tasks.length === 0) {
    console.log("(no tasks)");
    return;
  }
  table(
    ["ID", "STATUS", "SESSION", "TITLE"],
    resp.tasks.map((t) => [
      t.id,
      t.status,
      t.sessions[0]?.sessionId ?? "-",
      truncate(t.title ?? "", 48),
    ]),
    [36, 10, 36, 48],
  );
}

export async function get(c: Clients, id: string, json: boolean): Promise<void> {
  const resp = await c.task.getTask({ taskId: id }).catch(failWith);
  const t = resp.task;
  if (!t) failWith(new Error("response carried no task"));
  if (json) {
    printJson(taskJson(t));
    return;
  }
  detail([
    ["id", t.id],
    ["type", t.type],
    ["status", t.status],
    ["title", t.title],
    ["created_by", t.createdByUserId],
    ["created_at", t.createdAt],
  ]);
  for (const s of t.sessions) {
    console.log(
      `session         : ${s.sessionId} (${s.role ?? "primary"}, ${s.session?.status ?? "?"})`,
    );
  }
}

export async function remove(c: Clients, id: string): Promise<void> {
  await c.task.deleteTask({ taskId: id }).catch(failWith);
  console.log("deleted");
}
