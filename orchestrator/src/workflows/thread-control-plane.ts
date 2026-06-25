/**
 * Real ThreadControlPlane (ADR 0059 P2.7) — the session-lifecycle half of the
 * SlackThreadWorkflow's seams, wired to the orchestrator's stores + the
 * control-plane client. Construct once at init and pass to `setThreadControlPlane`.
 *
 * createSession reuses CreateTask's profile→session compilation
 * (`compileSessionCreateInput`) so a triggered session runs with the SAME
 * capabilities/network/secrets/skills as a UI task (no new privilege path),
 * and folds the trigger's system prompt into harness_env as
 * ENGRAM_APPEND_SYSTEM_PROMPT (the coordinator injects+persists+replays
 * harness_env — no coordinator change; the harness turns it into
 * --append-system-prompt). It then persists a `slack_thread` task owned by the
 * resolved engrams user (so existing CASL applies), with session-create+task
 * atomic via compensation. A replayed create step may make a stray session —
 * accepted (ADR Decision 9): it idle-evicts cheaply.
 */

import { log as rootLog } from "../log.ts";
import { getDb } from "../db/client.ts";
import { task as taskTable, taskSession as taskSessionTable } from "../db/schema.ts";
import { sessions as defaultSessions, images as defaultImages } from "../control-plane/client.ts";
import { makeProfileStore, type ProfileStore } from "../db/profiles.ts";
import { makeUserSecretStore } from "../db/user-secrets.ts";
import { makeConnectorStore } from "../db/connectors.ts";
import { compileSessionCreateInput, type SessionCreateInput } from "../rpc/session-compile.ts";
import { resolveEngramsUser } from "../integrations/slack-identity.ts";
import type { CustomConnectorSource } from "../connectors/registry.ts";
import type { ImagesClient } from "../rpc/profiles.ts";
import { config } from "../config.ts";
import type { ThreadControlPlane } from "./slack-thread.ts";

const log = rootLog.child({ component: "slack" });

/** A StringList map value (proto3 maps can't hold `repeated` directly). */
interface StringList {
  values: string[];
}

/** The control-plane session ops the thread workflow drives. */
export interface ThreadSessionsClient {
  createSession(req: SessionCreateInput): Promise<{ sessionId: string }>;
  sendPrompt(req: { sessionId: string; promptId?: string; text: string }): Promise<unknown>;
  answerQuestion(req: {
    sessionId: string;
    toolCallId: string;
    answers: Record<string, StringList>;
  }): Promise<unknown>;
  deleteSession(req: { sessionId: string }): Promise<unknown>;
}

export interface PersistTaskArgs {
  taskId: string;
  type: string;
  sessionId: string;
  profileId: string;
  ownerUserId: string;
  source: Record<string, unknown>;
}

export interface ThreadControlPlaneDeps {
  profiles?: ProfileStore;
  images?: ImagesClient;
  connectors?: CustomConnectorSource;
  secrets?: { get(userId: string, envVar: string): Promise<string | null> };
  sessions?: ThreadSessionsClient;
  resolveUser?: (provider: string, externalUserId: string) => Promise<string | null>;
  /** Atomically persist the task + its primary task_session. Default = a DB tx. */
  persistTask?: (args: PersistTaskArgs) => Promise<void>;
}

/** Build the production ThreadControlPlane; all deps injectable for tests. */
export function makeThreadControlPlane(deps: ThreadControlPlaneDeps = {}): ThreadControlPlane {
  const profiles = deps.profiles ?? makeProfileStore(getDb());
  const images = deps.images ?? (defaultImages as unknown as ImagesClient);
  const connectors = deps.connectors ?? { list: () => makeConnectorStore(getDb()).list() };
  const secrets = deps.secrets ?? makeUserSecretStore(getDb());
  const sessions = deps.sessions ?? (defaultSessions as unknown as ThreadSessionsClient);
  const resolveUser = deps.resolveUser ?? resolveEngramsUser;
  const persistTask = deps.persistTask ?? defaultPersistTask;

  return {
    resolveUser: (provider, externalUserId) => resolveUser(provider, externalUserId),
    getDefaultProfile: () => profiles.getDefault(),

    async createSession(input) {
      const profile = await profiles.getActive(input.profileId);
      if (!profile) throw new Error(`default profile ${input.profileId} not found or archived`);

      const sessionInput = await compileSessionCreateInput(
        profile,
        { images, connectors, resolveUserToken: (envVar) => secrets.get(input.ownerUserId, envVar) },
        {
          ...(input.prompt ? { prompt: input.prompt } : {}),
          ...(input.appendSystemPrompt
            ? { extraHarnessEnv: { ENGRAM_APPEND_SYSTEM_PROMPT: input.appendSystemPrompt } }
            : {}),
        },
      );

      const created = await sessions.createSession(sessionInput);

      const taskId = crypto.randomUUID();
      try {
        await persistTask({
          taskId,
          type: "slack_thread",
          sessionId: created.sessionId,
          profileId: profile.id,
          ownerUserId: input.ownerUserId,
          source: input.source,
        });
      } catch (err) {
        // Compensate: drop the orphan session so a retry starts clean.
        try {
          await sessions.deleteSession({ sessionId: created.sessionId });
        } catch (delErr) {
          log.error(
            { sessionId: created.sessionId, err: delErr },
            "slack: failed to delete orphan session after task-persist failure",
          );
        }
        throw err;
      }

      return { id: created.sessionId, webUrl: `${config.baseUrl}/sessions/${created.sessionId}` };
    },

    sendPrompt: async (sessionId, prompt, promptId) => {
      await sessions.sendPrompt({ sessionId, promptId, text: prompt });
    },

    answerQuestion: async (sessionId, toolCallId, answers) => {
      const wire: Record<string, StringList> = {};
      for (const [k, v] of Object.entries(answers)) wire[k] = { values: v };
      await sessions.answerQuestion({ sessionId, toolCallId, answers: wire });
    },
  };
}

/** Default task persistence: task + primary task_session in one transaction. */
async function defaultPersistTask(args: PersistTaskArgs): Promise<void> {
  const db = getDb();
  await db.transaction(async (tx) => {
    await tx.insert(taskTable).values({
      id: args.taskId,
      type: args.type,
      status: "open",
      createdByUserId: args.ownerUserId,
      source: args.source,
    });
    await tx.insert(taskSessionTable).values({
      taskId: args.taskId,
      sessionId: args.sessionId,
      role: "primary",
      profileId: args.profileId,
    });
  });
}
