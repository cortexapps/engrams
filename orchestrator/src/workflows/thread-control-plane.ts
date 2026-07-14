/**
 * Real ThreadControlPlane (ADR 0060 P2.7) — the session-lifecycle half of the
 * SlackThreadWorkflow's seams, wired to the orchestrator's stores + the
 * control-plane client. Construct once at init and pass to `setThreadControlPlane`.
 *
 * createTask reuses the shared create-a-task primitive (`createTaskWithSession`,
 * the same path the UI's CreateTask RPC uses) so a triggered session runs with
 * the SAME capabilities/network/secrets/skills as a UI chat task (no new
 * privilege path) and is NEVER created outside the task model. It records the
 * trigger ref on a `slack_thread` task owned by the resolved engrams user (so
 * existing CASL applies) and folds the trigger's system prompt into harness_env
 * as ENGRAM_APPEND_SYSTEM_PROMPT (the coordinator injects+persists+replays
 * harness_env — no coordinator change; the harness turns it into
 * --append-system-prompt). A replayed create step may make a stray session —
 * accepted (ADR Decision 9): it idle-evicts cheaply.
 */

import { getDb } from "../db/client.ts";
import {
  sessions as defaultSessions,
  images as defaultImages,
  harnessCatalog as defaultHarnessCatalog,
} from "../control-plane/client.ts";
import { makeProfileStore, type ProfileStore } from "../db/profiles.ts";
import { makeUserSecretStore } from "../db/user-secrets.ts";
import { makeConnectorStore } from "../db/connectors.ts";
import {
  createTaskWithSession,
  type Db,
  type EnsureListenerRow,
  type TaskSessionsClient,
  type HarnessCatalogClient,
} from "../rpc/task-create.ts";
import { resolveEngramsUser } from "../integrations/slack-identity.ts";
import type { CustomConnectorSource } from "../connectors/registry.ts";
import type { ImagesClient } from "../rpc/profiles.ts";
import { config } from "../config.ts";
import type { ThreadControlPlane } from "./slack-thread.ts";
import { ensureListenerRow as ensureProductionListenerRow } from "../listeners/lease-store.ts";

/** A StringList map value (proto3 maps can't hold `repeated` directly). */
interface StringList {
  values: string[];
}

/** The control-plane session ops the thread workflow drives. The create/delete
 *  half is the shared `TaskSessionsClient` (used by the create-task primitive);
 *  send/answer are the per-turn ops only the thread workflow needs. */
export interface ThreadSessionsClient extends TaskSessionsClient {
  sendPrompt(req: { sessionId: string; promptId?: string; text: string }): Promise<unknown>;
  answerQuestion(req: {
    sessionId: string;
    toolCallId: string;
    answers: Record<string, StringList>;
  }): Promise<unknown>;
}

export interface ThreadControlPlaneDeps {
  profiles?: ProfileStore;
  images?: ImagesClient;
  connectors?: CustomConnectorSource;
  harnessCatalog?: HarnessCatalogClient;
  secrets?: { get(userId: string, envVar: string): Promise<string | null> };
  sessions?: ThreadSessionsClient;
  resolveUser?: (provider: string, externalUserId: string) => Promise<string | null>;
  ensureListenerRow?: EnsureListenerRow;
  /** The Drizzle DB the task-persist transaction runs on. Default = the pool. */
  db?: Db;
}

/** Build the production ThreadControlPlane; all deps injectable for tests. */
export function makeThreadControlPlane(deps: ThreadControlPlaneDeps = {}): ThreadControlPlane {
  const profiles = deps.profiles ?? makeProfileStore(getDb());
  const images = deps.images ?? (defaultImages as unknown as ImagesClient);
  const connectors = deps.connectors ?? { list: () => makeConnectorStore(getDb()).list() };
  const harnessCatalog =
    deps.harnessCatalog ?? (defaultHarnessCatalog as unknown as HarnessCatalogClient);
  const secrets = deps.secrets ?? makeUserSecretStore(getDb());
  const sessions = deps.sessions ?? (defaultSessions as unknown as ThreadSessionsClient);
  const resolveUser = deps.resolveUser ?? resolveEngramsUser;
  const ensureListenerRow =
    deps.ensureListenerRow ?? ensureProductionListenerRow;
  const db = deps.db ?? getDb();

  return {
    resolveUser: (provider, externalUserId) => resolveUser(provider, externalUserId),
    getDefaultProfile: () => profiles.getDefault(),

    async createTask(input) {
      const { sessionId } = await createTaskWithSession(
        {
          profiles,
          images,
          connectors,
          harnessCatalog,
          sessions,
          ensureListenerRow,
          secrets,
          db,
        },
        {
          type: "slack_thread",
          ownerUserId: input.ownerUserId,
          profileId: input.profileId,
          source: input.source,
          ...(input.prompt ? { prompt: input.prompt } : {}),
          ...(input.appendSystemPrompt
            ? { extraHarnessEnv: { ENGRAM_APPEND_SYSTEM_PROMPT: input.appendSystemPrompt } }
            : {}),
        },
      );
      return { id: sessionId, webUrl: `${config.baseUrl}/sessions/${sessionId}` };
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
