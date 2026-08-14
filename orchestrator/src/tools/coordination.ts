/** Recursive session coordination tools (ADR 0113). */

import { createHash } from "node:crypto";
import { Code, ConnectError } from "@connectrpc/connect";
import { and, eq, ne, sql } from "drizzle-orm";
import { z } from "zod";

import { config } from "../config.ts";
import {
  harnessCatalog,
  images,
  oauthCredential,
  orgSecret,
  sessions,
} from "../control-plane/client.ts";
import { getDb } from "../db/client.ts";
import { makeConnectorStore } from "../db/connectors.ts";
import type {
  IntegrationConnectionRow,
  IntegrationConnectionStore,
} from "../db/integration-connections.ts";
import { makeUserSecretStore } from "../db/user-secrets.ts";
import { makeUserIdentityStore } from "../db/users.ts";
import { coordinationOperation, task, taskSession, type TaskLaunchPolicy } from "../db/schema.ts";
import { OauthSubjectKind } from "../gen/engram/app/v1/oauth_pb.ts";
import { integrationSnapshotHash } from "../integrations/grants.ts";
import {
  compileSessionCreateInput,
  type HarnessCatalogClient,
  type SessionCreateInput,
} from "../rpc/task-create.ts";
import type { ImagesClient } from "../rpc/profiles.ts";
import { tools as productionTools, type ToolContext, type ToolRegistry } from "./registry.ts";

const TaskName = z
  .string()
  .regex(/^[a-z0-9][a-z0-9_-]{0,63}$/, "must match [a-z0-9][a-z0-9_-]{0,63}");
const IdempotencyKey = z.string().min(1).max(200);
const TaskPath = z.string().regex(/^[a-z0-9][a-z0-9_-]{0,63}(\/[a-z0-9][a-z0-9_-]{0,63})*$/);
const GuestFilePath = z
  .string()
  .min(2)
  .max(4096)
  .refine(
    (path) =>
      path.startsWith("/") &&
      !path.includes("\0") &&
      path
        .slice(1)
        .split("/")
        .every((component) => component !== "" && component !== "." && component !== ".."),
    "must be a normalized absolute guest file path",
  )
  .describe(
    "A normalized absolute path to a file in the caller session, such as /tmp/results.json",
  );

const SpawnInput = z.object({
  task_name: TaskName,
  message: z.string().min(1),
  idempotency_key: IdempotencyKey,
  file_paths: z.array(GuestFilePath).optional(),
  harness_override: z.string().min(1).optional(),
  model_override: z.string().min(1).optional(),
  effort_override: z.string().min(1).optional(),
});

const SendInput = z.object({
  session_id: z.string().uuid(),
  message: z.string().min(1),
  idempotency_key: IdempotencyKey,
  file_paths: z.array(GuestFilePath).optional(),
});

const SessionEventSchema = z.object({
  cursor: z.string(),
  type: z.string(),
  payload: z.unknown(),
});

const SessionUpdateSchema = z.object({
  session_id: z.string(),
  state: z.string(),
  next_cursor: z.string(),
  events: z.array(SessionEventSchema),
});

interface TaskRow {
  id: string;
  type: string;
  status: string;
  parentTaskId: string | null;
  rootTaskId: string | null;
  canonicalTaskName: string | null;
  createdByUserId: string | null;
  launchPolicy: TaskLaunchPolicy | null;
}

function requestHash(value: unknown): string {
  return createHash("sha256").update(JSON.stringify(value)).digest("hex");
}

async function callerTask(ctx: ToolContext): Promise<TaskRow> {
  if (!ctx.taskId) throw new Error("caller session has no task");
  const rows = await getDb()
    .select({
      id: task.id,
      type: task.type,
      status: task.status,
      parentTaskId: task.parentTaskId,
      rootTaskId: task.rootTaskId,
      canonicalTaskName: task.canonicalTaskName,
      createdByUserId: task.createdByUserId,
      launchPolicy: task.launchPolicy,
    })
    .from(task)
    .where(eq(task.id, ctx.taskId))
    .limit(1);
  if (!rows[0]) throw new Error("caller task does not exist");
  return rows[0];
}

async function descendantForSession(ctx: ToolContext, sessionId: string): Promise<TaskRow> {
  const caller = await callerTask(ctx);
  const rows = await getDb()
    .select({
      id: task.id,
      type: task.type,
      status: task.status,
      parentTaskId: task.parentTaskId,
      rootTaskId: task.rootTaskId,
      canonicalTaskName: task.canonicalTaskName,
      createdByUserId: task.createdByUserId,
      launchPolicy: task.launchPolicy,
    })
    .from(taskSession)
    .innerJoin(task, eq(taskSession.taskId, task.id))
    .where(eq(taskSession.sessionId, sessionId))
    .limit(1);
  const target = rows[0];
  const callerRoot = caller.rootTaskId ?? caller.id;
  const targetRoot = target?.rootTaskId ?? target?.id;
  const prefix = caller.canonicalTaskName == null ? "" : `${caller.canonicalTaskName}/`;
  if (
    !target ||
    target.id === caller.id ||
    targetRoot !== callerRoot ||
    target.canonicalTaskName == null ||
    !target.canonicalTaskName.startsWith(prefix)
  ) {
    throw new Error("target session is not a proper descendant of the caller");
  }
  return target;
}

async function descendants(ctx: ToolContext): Promise<Array<TaskRow & { sessionId: string }>> {
  const caller = await callerTask(ctx);
  const rootTaskId = caller.rootTaskId ?? caller.id;
  const prefix = caller.canonicalTaskName == null ? "" : `${caller.canonicalTaskName}/`;
  const rows = await getDb()
    .select({
      id: task.id,
      type: task.type,
      status: task.status,
      parentTaskId: task.parentTaskId,
      rootTaskId: task.rootTaskId,
      canonicalTaskName: task.canonicalTaskName,
      createdByUserId: task.createdByUserId,
      launchPolicy: task.launchPolicy,
      sessionId: taskSession.sessionId,
    })
    .from(task)
    .innerJoin(taskSession, eq(taskSession.taskId, task.id))
    .where(and(eq(task.rootTaskId, rootTaskId), ne(task.id, caller.id)));
  return rows.filter(
    (row) => row.canonicalTaskName != null && row.canonicalTaskName.startsWith(prefix),
  );
}

function snapshotConnectionStore(policy: TaskLaunchPolicy): IntegrationConnectionStore {
  const rows: IntegrationConnectionRow[] = policy.integrationConnections.map((connection) => ({
    ...connection,
    isDefault: false,
    enabled: true,
    testedAt: null,
    createdAt: new Date(0),
    updatedAt: new Date(0),
  }));
  const byId = new Map(rows.map((row) => [row.id, row]));
  const unsupported = async (): Promise<never> => {
    throw new Error("launch snapshot connection store is read-only");
  };
  return {
    list: async () => rows,
    get: async (id) => byId.get(id) ?? null,
    getMany: async (ids) => ids.flatMap((id) => byId.get(id) ?? []),
    getDefault: async (provider) => rows.find((row) => row.provider === provider) ?? null,
    create: unsupported,
    update: unsupported,
    delete: unsupported,
    markTested: unsupported,
    setEnabled: unsupported,
    ensureDefault: unsupported,
  };
}

async function compileChildInput(
  parent: TaskRow,
  args: z.output<typeof SpawnInput>,
  sessionId: string,
): Promise<SessionCreateInput> {
  const policy = parent.launchPolicy;
  if (!policy)
    throw new Error("tasks created before launch-policy snapshots cannot spawn children");
  if (!parent.createdByUserId) throw new Error("sub-session spawning requires an owning user");
  const db = getDb();
  const secrets = makeUserSecretStore(db);
  const identity = await makeUserIdentityStore(db).getIdentity(parent.createdByUserId);
  // ADR 0115: the child ships the parent's FROZEN policy, so the user-scoped
  // gate must follow what that policy actually stamped — not the profile
  // toggle. A service-account parent compiled org authority (no `oauth_user`
  // entries); re-deriving from the toggle would wrongly block its spawns.
  const userStampedConnections = (() => {
    try {
      const parsed = JSON.parse(policy.integrationPolicyJson || "{}") as {
        injects?: Array<{ mint_source?: { oauth_user?: { connection_id?: string } } | null }>;
      };
      return new Set(
        (parsed.injects ?? [])
          .map((inject) => inject.mint_source?.oauth_user?.connection_id)
          .filter((id): id is string => typeof id === "string"),
      );
    } catch {
      return new Set<string>();
    }
  })();
  const syntheticProfile = {
    id: policy.profileId,
    name: "launch snapshot",
    description: "",
    icon: "",
    imageId: "launch-snapshot",
    harness: policy.harness,
    modelRouter: policy.modelRouter ?? null,
    model: policy.model ?? null,
    effort: policy.effort ?? null,
    includeUserTokens: policy.includeUserTokens,
    envVars: structuredClone(policy.envVars),
    skills: [...policy.skills],
    integrationGrants: structuredClone(policy.integrationGrants).map((grant) =>
      userStampedConnections.has(grant.connectionId)
        ? { ...grant, credentialScope: "user" as const }
        : { ...grant, credentialScope: "org" as const },
    ),
    network: structuredClone(policy.network),
    secrets: structuredClone(policy.secrets),
    repos: structuredClone(policy.repos),
    portExposures: [...policy.portExposures],
    designation: null,
    createdAt: new Date(0),
    updatedAt: new Date(0),
    deletedAt: null,
  };
  const imagesClient: ImagesClient = {
    listEnabledImages: async () => ({
      images: [{ id: "launch-snapshot", imageUri: policy.imageUri }],
    }),
  };
  const input = await compileSessionCreateInput(
    syntheticProfile,
    {
      images: imagesClient,
      connectors: { list: () => makeConnectorStore(db).list() },
      harnessCatalog: harnessCatalog as unknown as HarnessCatalogClient,
      toolRegistry: productionTools,
      resolveUserToken: (envVar) => secrets.get(parent.createdByUserId!, envVar),
      resolveAllUserTokens: () => secrets.getAll(parent.createdByUserId!),
      oauthSubject: { kind: OauthSubjectKind.USER, id: parent.createdByUserId },
      hasOAuthCredential: async (provider) => {
        const response = await oauthCredential.listCredentials({
          subject: { kind: OauthSubjectKind.USER, id: parent.createdByUserId! },
        });
        return response.credentials.some(
          (credential) => credential.provider === provider && credential.connected,
        );
      },
      listUserConnectorCredentials: async () => {
        const response = await oauthCredential.listCredentials({
          subject: { kind: OauthSubjectKind.USER_CONNECTOR, id: parent.createdByUserId! },
        });
        return response.credentials.map((credential) => ({
          provider: credential.provider,
          status: credential.status,
        }));
      },
      orgSecret,
      connections: snapshotConnectionStore(policy),
    },
    {
      ...(args.harness_override ? { harness: args.harness_override } : {}),
      ...(args.model_override ? { model: args.model_override } : {}),
      ...(args.effort_override ? { effort: args.effort_override } : {}),
      ...(identity ? { owner: identity } : {}),
      excludeHumanInteractionTools: true,
    },
  );
  return {
    ...input,
    requestedSessionId: sessionId,
    imageUri: policy.imageUri,
    selectedSkills: [...policy.skills],
    capabilities: [...policy.capabilities],
    integrationPolicyJson: policy.integrationPolicyJson,
    integrationGrants: structuredClone(policy.integrationGrants),
    integrationConnections: structuredClone(policy.integrationConnections),
    prompt: undefined,
    harnessMode: undefined,
  };
}

async function reserve(
  ctx: ToolContext,
  operation: string,
  idempotencyKey: string,
  request: unknown,
  ids: { taskId?: string; sessionId?: string; promptId?: string },
) {
  const db = getDb();
  const hash = requestHash(request);
  await db
    .insert(coordinationOperation)
    .values({
      callerSessionId: ctx.sessionId,
      operation,
      idempotencyKey,
      requestHash: hash,
      reservedTaskId: ids.taskId,
      reservedSessionId: ids.sessionId,
      promptId: ids.promptId,
    })
    .onConflictDoNothing();
  const rows = await db
    .select()
    .from(coordinationOperation)
    .where(
      and(
        eq(coordinationOperation.callerSessionId, ctx.sessionId),
        eq(coordinationOperation.operation, operation),
        eq(coordinationOperation.idempotencyKey, idempotencyKey),
      ),
    )
    .limit(1);
  const row = rows[0];
  if (!row) throw new Error("coordination operation reservation disappeared");
  if (row.requestHash !== hash) {
    throw new ConnectError(
      "idempotency key was already used with different arguments",
      Code.AlreadyExists,
    );
  }
  return row;
}

async function finish(
  ctx: ToolContext,
  operation: string,
  idempotencyKey: string,
  result: Record<string, unknown>,
) {
  await getDb()
    .update(coordinationOperation)
    .set({ status: "complete", result, error: null, updatedAt: new Date() })
    .where(
      and(
        eq(coordinationOperation.callerSessionId, ctx.sessionId),
        eq(coordinationOperation.operation, operation),
        eq(coordinationOperation.idempotencyKey, idempotencyKey),
      ),
    );
}

async function fail(ctx: ToolContext, operation: string, idempotencyKey: string, error: unknown) {
  await getDb()
    .update(coordinationOperation)
    .set({
      status: "failed",
      error: error instanceof Error ? error.message : String(error),
      updatedAt: new Date(),
    })
    .where(
      and(
        eq(coordinationOperation.callerSessionId, ctx.sessionId),
        eq(coordinationOperation.operation, operation),
        eq(coordinationOperation.idempotencyKey, idempotencyKey),
      ),
    );
}

async function readUpdate(sessionId: string, afterCursor?: string, limit = 100) {
  const after = afterCursor == null || afterCursor === "" ? undefined : BigInt(afterCursor);
  const refs = await getDb()
    .select({ taskId: taskSession.taskId, taskStatus: task.status })
    .from(taskSession)
    .innerJoin(task, eq(task.id, taskSession.taskId))
    .where(eq(taskSession.sessionId, sessionId))
    .limit(1);
  const sessionResponse = await sessions.getSession({ sessionId }).catch(() => null);
  const eventsResponse = await sessions
    .listSessionEvents({
      sessionId,
      ...(after !== undefined ? { afterIdx: after } : {}),
      limit: BigInt(limit),
    })
    .catch(() => null);
  if (!eventsResponse) {
    return {
      session_id: sessionId,
      state: refs[0]?.taskStatus ?? "unknown",
      next_cursor: afterCursor ?? "0",
      events: [],
    };
  }
  const relevant = eventsResponse.events.filter(
    (event) =>
      event.kind === "agent_message" ||
      event.kind.startsWith("run_") ||
      event.kind.includes("error") ||
      event.kind.includes("failed"),
  );
  let projectedState: "working" | "done" | "failed" | "open" | undefined;
  switch (sessionResponse?.session?.status) {
    case "completed":
      projectedState = "done";
      break;
    case "failed":
    case "dead":
    case "host_lost":
      projectedState = "failed";
      break;
  }
  for (const event of relevant) {
    if (event.kind === "run_started") projectedState = "working";
    if (event.kind === "run_interrupted") projectedState = "open";
    if (event.kind === "run_completed") {
      try {
        const payload = JSON.parse(event.payloadJson) as { ok?: boolean };
        projectedState = payload.ok === false ? "failed" : "done";
      } catch {
        projectedState = "done";
      }
    }
  }
  if (projectedState) {
    if (refs[0]) {
      await getDb()
        .update(task)
        .set({ status: projectedState })
        .where(
          and(
            eq(task.id, refs[0].taskId),
            eq(task.type, "subsession"),
            ne(task.status, "cancelled"),
          ),
        );
    }
  }
  return {
    session_id: sessionId,
    state: projectedState ?? refs[0]?.taskStatus ?? sessionResponse?.session?.status ?? "unknown",
    next_cursor: eventsResponse.nextAfterIdx.toString(),
    events: relevant.map((event) => {
      let payload: unknown = event.payloadJson;
      try {
        payload = JSON.parse(event.payloadJson);
      } catch {
        // Keep malformed historical payloads visible as text.
      }
      return {
        cursor: (event.idx ?? eventsResponse.nextAfterIdx).toString(),
        type: event.kind,
        payload,
      };
    }),
  };
}

export function registerCoordinationTools(registry: ToolRegistry): void {
  registry.register({
    name: "spawn_session",
    description:
      "Spawn a named child session. Optional file_paths may name files anywhere in this session and are copied to the same absolute paths before the message is sent.",
    input: SpawnInput,
    output: z.object({ session_id: z.string(), canonical_task_name: z.string() }),
    handling: "handled",
    execution: "sync",
    handler: async (ctx, args) => {
      const initialIds = {
        taskId: crypto.randomUUID(),
        sessionId: crypto.randomUUID(),
        promptId: crypto.randomUUID(),
      };
      const operation = await reserve(ctx, "spawn_session", args.idempotency_key, args, initialIds);
      if (operation.status === "complete" && operation.result) {
        return {
          session_id: String(operation.result.session_id),
          canonical_task_name: String(operation.result.canonical_task_name),
        };
      }
      try {
        const taskId = operation.reservedTaskId!;
        const sessionId = operation.reservedSessionId!;
        const promptId = operation.promptId!;
        const parent = await callerTask(ctx);
        const canonicalName = parent.canonicalTaskName
          ? `${parent.canonicalTaskName}/${args.task_name}`
          : args.task_name;
        if (canonicalName.split("/").length > config.subSessionMaxDepth) {
          throw new Error(`sub-session depth exceeds ${config.subSessionMaxDepth}`);
        }
        const rootTaskId = parent.rootTaskId ?? parent.id;
        const input = await compileChildInput(parent, args, sessionId);
        const policy = parent.launchPolicy!;
        const childPolicy: TaskLaunchPolicy = {
          ...structuredClone(policy),
          harness: input.harness ?? policy.harness,
          ...(input.modelRouter ? { modelRouter: input.modelRouter } : {}),
          ...(input.model ? { model: input.model } : {}),
          ...(input.effort ? { effort: input.effort } : {}),
        };
        await getDb().transaction(async (tx) => {
          // Serialize reservations within one tree. This closes the race where
          // concurrent, differently named spawns both observe seven children
          // and create the ninth descendant.
          await tx.execute(
            sql`select ${task.id} from ${task} where ${task.id} = ${rootTaskId} for update`,
          );
          const liveDescendants = await tx
            .select({ id: task.id })
            .from(task)
            .where(
              and(
                eq(task.rootTaskId, rootTaskId),
                ne(task.id, rootTaskId),
                ne(task.id, taskId),
                ne(task.status, "cancelled"),
              ),
            );
          if (liveDescendants.length >= config.subSessionMaxDescendants) {
            throw new Error(
              `task tree already has ${config.subSessionMaxDescendants} live descendants`,
            );
          }
          const inserted = await tx
            .insert(task)
            .values({
              id: taskId,
              type: "subsession",
              title: args.task_name,
              status: "open",
              createdByUserId: parent.createdByUserId,
              source: {},
              harness: input.harness ?? null,
              modelRouter: input.modelRouter ?? null,
              model: input.model ?? null,
              effort: input.effort ?? null,
              parentTaskId: parent.id,
              rootTaskId,
              localTaskName: args.task_name,
              canonicalTaskName: canonicalName,
              spawningSessionId: ctx.sessionId,
              launchPolicy: childPolicy,
            })
            .onConflictDoNothing()
            .returning({ id: task.id });
          if (inserted.length === 0) {
            const existing = await tx
              .select({ id: task.id })
              .from(task)
              .where(
                and(eq(task.rootTaskId, rootTaskId), eq(task.canonicalTaskName, canonicalName)),
              )
              .limit(1);
            if (existing[0]?.id !== taskId) {
              throw new ConnectError(
                `canonical task name ${canonicalName} was already used in this tree`,
                Code.AlreadyExists,
              );
            }
          }
          await tx
            .insert(taskSession)
            .values({
              taskId,
              sessionId,
              role: "primary",
              profileId: policy.profileId,
              capabilities: [...policy.capabilities],
              integrationGrants: structuredClone(policy.integrationGrants),
              integrationConnections: structuredClone(policy.integrationConnections),
              integrationPrincipalId: parent.createdByUserId,
              integrationSnapshotHash: integrationSnapshotHash({
                profileId: policy.profileId,
                integrationGrants: policy.integrationGrants,
                integrationConnections: policy.integrationConnections,
              }),
            })
            .onConflictDoNothing();
        });
        try {
          await sessions.createSession(input);
        } catch (error) {
          try {
            await sessions.getSession({ sessionId });
          } catch {
            throw error;
          }
        }
        if ((args.file_paths?.length ?? 0) > 0) {
          await sessions.copyFiles({
            sourceSessionId: ctx.sessionId,
            targetSessionId: sessionId,
            paths: args.file_paths!,
          });
        }
        await getDb().update(task).set({ status: "working" }).where(eq(task.id, taskId));
        await sessions.sendPrompt({ sessionId, text: args.message, promptId });
        const result = { session_id: sessionId, canonical_task_name: canonicalName };
        await finish(ctx, "spawn_session", args.idempotency_key, result);
        return result;
      } catch (error) {
        await fail(ctx, "spawn_session", args.idempotency_key, error).catch(() => {});
        throw error;
      }
    },
  });

  registry.register({
    name: "send_session_message",
    description:
      "Send a message to a descendant session. Optional file_paths may name files anywhere in this session and are copied to the same absolute paths before the normal prompt path accepts the message.",
    input: SendInput,
    output: z.object({ session_id: z.string(), accepted: z.boolean() }),
    handling: "handled",
    execution: "sync",
    handler: async (ctx, args) => {
      const target = await descendantForSession(ctx, args.session_id);
      const operation = await reserve(ctx, "send_session_message", args.idempotency_key, args, {
        sessionId: args.session_id,
        promptId: crypto.randomUUID(),
      });
      if (operation.status === "complete" && operation.result) {
        return {
          session_id: String(operation.result.session_id),
          accepted: operation.result.accepted === true,
        };
      }
      try {
        if ((args.file_paths?.length ?? 0) > 0) {
          await sessions.copyFiles({
            sourceSessionId: ctx.sessionId,
            targetSessionId: args.session_id,
            paths: args.file_paths!,
          });
        }
        await sessions.sendPrompt({
          sessionId: args.session_id,
          text: args.message,
          promptId: operation.promptId!,
        });
        await getDb().update(task).set({ status: "working" }).where(eq(task.id, target.id));
        const result = { session_id: args.session_id, accepted: true };
        await finish(ctx, "send_session_message", args.idempotency_key, result);
        return result;
      } catch (error) {
        await fail(ctx, "send_session_message", args.idempotency_key, error).catch(() => {});
        throw error;
      }
    },
  });

  registry.register({
    name: "read_session",
    description: "Read assistant output and run-state updates from a descendant session.",
    input: z.object({
      session_id: z.string().uuid(),
      after_cursor: z
        .string()
        .regex(/^-?\d+$/)
        .optional(),
      limit: z.number().int().min(1).max(500).optional(),
    }),
    output: SessionUpdateSchema,
    handling: "handled",
    execution: "sync",
    handler: async (ctx, args) => {
      await descendantForSession(ctx, args.session_id);
      return readUpdate(args.session_id, args.after_cursor, args.limit);
    },
  });

  registry.register({
    name: "interrupt_session",
    description: "Interrupt only the selected descendant session.",
    input: z.object({ session_id: z.string().uuid() }),
    output: z.object({ session_id: z.string(), interrupted: z.boolean() }),
    handling: "handled",
    execution: "sync",
    handler: async (ctx, args) => {
      const target = await descendantForSession(ctx, args.session_id);
      await sessions.interrupt({ sessionId: args.session_id, source: "coordination-tool" });
      await getDb().update(task).set({ status: "open" }).where(eq(task.id, target.id));
      return { session_id: args.session_id, interrupted: true };
    },
  });

  registry.register({
    name: "terminate_session",
    description: "Terminate only the selected descendant session.",
    input: z.object({ session_id: z.string().uuid() }),
    output: z.object({ session_id: z.string(), terminated: z.boolean() }),
    handling: "handled",
    execution: "sync",
    handler: async (ctx, args) => {
      const target = await descendantForSession(ctx, args.session_id);
      await sessions.deleteSession({ sessionId: args.session_id });
      await getDb().update(task).set({ status: "cancelled" }).where(eq(task.id, target.id));
      return { session_id: args.session_id, terminated: true };
    },
  });

  registry.register({
    name: "list_sessions",
    description: "List descendant sessions, optionally below a canonical task-name prefix.",
    input: z.object({ path_prefix: TaskPath.optional() }),
    output: z.object({
      sessions: z.array(
        z.object({
          session_id: z.string(),
          canonical_task_name: z.string(),
          state: z.string(),
        }),
      ),
    }),
    handling: "handled",
    execution: "sync",
    handler: async (ctx, args) => {
      const rows = (await descendants(ctx)).filter(
        (row) =>
          !args.path_prefix ||
          row.canonicalTaskName === args.path_prefix ||
          row.canonicalTaskName!.startsWith(`${args.path_prefix}/`),
      );
      return {
        sessions: rows.map((row) => ({
          session_id: row.sessionId,
          canonical_task_name: row.canonicalTaskName!,
          state: row.status,
        })),
      };
    },
  });

  registry.register({
    name: "wait_sessions",
    description:
      "Wait until one descendant session has a relevant event after its cursor. Defaults to all descendants and 30 seconds; the hard limit is 120 seconds.",
    input: z.object({
      session_ids: z.array(z.string().uuid()).optional(),
      after_cursors: z.record(z.string(), z.string().regex(/^-?\d+$/)).optional(),
      timeout_ms: z.number().int().min(0).max(120_000).optional(),
    }),
    output: z.object({ updates: z.array(SessionUpdateSchema), timed_out: z.boolean() }),
    handling: "handled",
    execution: "sync",
    handler: async (ctx, args) => {
      const allowed = await descendants(ctx);
      const allowedIds = new Set(allowed.map((row) => row.sessionId));
      const ids = args.session_ids ?? [...allowedIds];
      for (const id of ids) {
        if (!allowedIds.has(id)) throw new Error("wait target is not a proper descendant");
      }
      const timeoutMs = args.timeout_ms ?? 30_000;
      const deadline = Date.now() + timeoutMs;
      do {
        const updates = await Promise.all(
          ids.map((id) => readUpdate(id, args.after_cursors?.[id], 100)),
        );
        const changed = updates.filter((update) => update.events.length > 0);
        if (changed.length > 0) return { updates: changed, timed_out: false };
        if (Date.now() >= deadline) return { updates, timed_out: true };
        await new Promise((resolve) => setTimeout(resolve, Math.min(250, deadline - Date.now())));
      } while (true);
    },
  });
}
