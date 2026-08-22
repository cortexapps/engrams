/** The automation interpreter workflow (ADR 0119).
 *
 * The registered body is deliberately thin: DBOS derives the application
 * version from this function's source (it does not recurse into helpers), so
 * the interpreter and block executors can evolve without rotating the
 * version. The ENGINE_STEP_CONTRACT literal below is the guard: any change to
 * step naming, step order, recv semantics, or the finalize position MUST bump
 * it, which rotates the version, strands in-flight executions loudly, and
 * lets the sweep (adopt, 48h) fail them instead of replaying them through
 * changed semantics. The golden step-sequence test in
 * src/automations/engine/__tests__/interpreter.test.ts enforces the contract
 * at review time. Additive changes (new block types, new outcome fields) keep
 * the contract.
 */


import { DBOS } from "@dbos-inc/dbos-sdk";
import { Code, ConnectError } from "@connectrpc/connect";

import {
  sessions as defaultSessions,
  images as defaultImages,
  harnessCatalog as defaultHarnessCatalog,
} from "../control-plane/client.ts";
import {
  makeAutomationEngineStore,
  type AutomationEngineStore,
} from "../db/automations.ts";
import { getDb } from "../db/client.ts";
import { makeConnectorStore } from "../db/connectors.ts";
import { makeProfileStore } from "../db/profiles.ts";
import {
  createSessionForExistingTask,
  registerSessionListener,
  truncatePrompt,
  type CreateSessionForExistingTaskParams,
  type HarnessCatalogClient,
} from "../rpc/task-create.ts";
import type { ImagesClient } from "../rpc/profiles.ts";
import { runExec } from "../exec/durable-exec.ts";
import { stageFiles } from "../exec/stage-files.ts";
import { AUTOMATION_TOPIC, type AutomationInbox } from "../automations/engine/inbox.ts";
import { interpretAutomation, type EngineRunResult } from "../automations/engine/interpreter.ts";
import type { EngineDeps, EngineSessionOps } from "../automations/engine/deps.ts";
import { makeCodeBlockRuntime } from "../automations/code/runtime.ts";
import { makeIntegrationActionRuntime } from "../automations/actions/runtime.ts";

export interface AutomationRunWorkflowInput {
  runId: string;
  automationId: string;
}

export interface AutomationRunWorkflowDeps {
  engine?: EngineDeps;
}

type CreateExistingSession = (
  params: CreateSessionForExistingTaskParams,
) => Promise<{ sessionId: string }>;

function productionImagesClient(): ImagesClient {
  return {
    async listEnabledImages(req) {
      const response = await defaultImages.listEnabledImages(req);
      return {
        images: response.images.map((image) => ({
          id: image.id,
          imageUri: image.imageUri,
        })),
      };
    },
  };
}

function productionHarnessCatalogClient(): HarnessCatalogClient {
  return {
    async listHarnesses(req) {
      const response = await defaultHarnessCatalog.listHarnesses(req);
      return {
        harnesses: response.harnesses.map((harness) => ({
          name: harness.name,
          ...(harness.descriptor ? { descriptor: harness.descriptor } : {}),
        })),
      };
    },
  };
}

export interface ProductionSessionOpsDeps {
  store?: AutomationEngineStore;
  createSessionForExistingTask?: CreateExistingSession;
  registerListener?: (sessionId: string) => Promise<void>;
}

/** EngineSessionOps over the control plane. The binding row lands BEFORE the
 * listener registration (the review-control-plane ordering), so no session
 * event ever arrives without a mailbox. */
export function makeProductionSessionOps(deps: ProductionSessionOpsDeps = {}): EngineSessionOps {
  let resolvedStore = deps.store;
  const store = () => (resolvedStore ??= makeAutomationEngineStore());
  let createExistingSession = deps.createSessionForExistingTask;
  const createSession = (): CreateExistingSession => {
    if (createExistingSession) return createExistingSession;
    const database = getDb();
    createExistingSession = (params) =>
      createSessionForExistingTask(
        {
          profiles: makeProfileStore(database),
          images: productionImagesClient(),
          connectors: { list: () => makeConnectorStore(database).list() },
          harnessCatalog: productionHarnessCatalogClient(),
          sessions: defaultSessions,
          // With no owner, the shared helper deliberately ignores personal
          // tokens and selects the harness's programmatic org credential.
          secrets: { get: async () => null, getAll: async () => ({}) },
          db: database,
        },
        params,
      );
    return createExistingSession;
  };

  return {
    async createSession(input) {
      const taskId = await store().ensureAutomationTask({
        runId: input.runId,
        automationId: input.automationId,
        title: input.title ?? truncatePrompt(input.prompt),
        source: {
          provider: "automation",
          automationId: input.automationId,
          runId: input.runId,
        },
      });
      // A replayed/retried primary launch reuses the task's primary session.
      if (input.role === "primary") {
        const existing = await store().getAutomationTaskSession(input.runId);
        if (existing !== null) return { sessionId: existing, taskId };
      }
      const { sessionId } = await createSession()({
        taskId,
        taskType: "automation",
        profileId: input.profileId,
        integrationPrincipalId: `automation:${input.automationId}`,
        role: input.role,
        prompt: input.prompt,
        ...(input.harnessMode !== undefined ? { harnessMode: input.harnessMode } : {}),
        ...(input.harness !== undefined ? { harness: input.harness } : {}),
        ...(input.model !== undefined ? { model: input.model } : {}),
        ...(input.modelRouter !== undefined ? { modelRouter: input.modelRouter } : {}),
        ...(input.effort !== undefined ? { effort: input.effort } : {}),
        ...(input.capabilityOverride !== undefined
          ? { capabilityOverride: input.capabilityOverride }
          : {}),
        ...(input.networkOverride !== undefined ? { networkOverride: input.networkOverride } : {}),
        ...(input.dropProfileSecretsAndEnv !== undefined
          ? { dropProfileSecretsAndEnv: input.dropProfileSecretsAndEnv }
          : {}),
        ...(input.appendSystemPrompt !== undefined
          ? { appendSystemPrompt: input.appendSystemPrompt }
          : {}),
        registerListener: false,
      });
      await store().recordSessionBinding({
        sessionId,
        runId: input.runId,
        blockId: input.blockId,
        role: input.role,
        keep: input.keep,
      });
      const register =
        deps.registerListener ?? ((id: string) => registerSessionListener(getDb(), id));
      await register(sessionId);
      if (input.role === "primary") {
        await store().recordRunLaunch({
          runId: input.runId,
          taskId,
          sessionId,
          prompt: input.prompt,
          title: input.title,
        });
      }
      return { sessionId, taskId };
    },

    async sendPrompt(sessionId, promptId, text, harnessMode) {
      // ADR 0107: a mid-run prompt may select a harness mode (e.g. "plan");
      // SendPromptRequest carries it as an optional field.
      await defaultSessions.sendPrompt({
        sessionId,
        promptId,
        text,
        ...(harnessMode !== undefined ? { harnessMode } : {}),
      });
    },

    async endSession(sessionId) {
      try {
        await defaultSessions.deleteSession({ sessionId });
      } catch (error) {
        if (error instanceof ConnectError && error.code === Code.NotFound) return;
        throw error;
      }
    },

    async exec(sessionId, command, options) {
      const result = await runExec(defaultSessions, sessionId, command, options);
      return {
        exitStatus: result.exitStatus,
        stdout: result.stdout,
        stderr: result.stderr,
      };
    },

    async writeFiles(sessionId, files) {
      // Shared WriteFile staging (exec/stage-files.ts): content-addressed by
      // sha256 so a replay is idempotent; per-file error capture.
      return stageFiles(
        defaultSessions,
        sessionId,
        files.map((file) => ({
          path: file.path,
          content: new TextEncoder().encode(file.content),
          mode: file.mode ?? 0o644,
        })),
      );
    },
  };
}

function productionEngineDeps(): EngineDeps {
  const store = makeAutomationEngineStore();
  return {
    step: (fn, name) => DBOS.runStep(fn, { name }),
    recv: (topic, timeoutSeconds) => DBOS.recv<AutomationInbox>(topic, timeoutSeconds),
    store,
    sessions: makeProductionSessionOps({ store }),
    clock: { nowMs: () => Date.now() },
    code: makeCodeBlockRuntime(),
    integrationActions: makeIntegrationActionRuntime(),
    async startQueuedRun(runId) {
      const run = await store.getRun(runId);
      if (!run) return;
      await DBOS.startWorkflow(automationRunWorkflow, { workflowID: runId })({
        runId,
        automationId: run.automationId,
      });
    },
  };
}

// Re-exported so dispatch/scheduler name the topic without importing engine
// internals directly.
export { AUTOMATION_TOPIC };

export async function automationRunWorkflowImpl(
  input: AutomationRunWorkflowInput,
  deps: AutomationRunWorkflowDeps = {},
): Promise<EngineRunResult> {
  // ADR 0119 D2: bump on ANY change to step naming, step order, recv
  // semantics, or finalize position anywhere in the engine. The literal lives
  // in this registered body so the bump rotates the DBOS application version.
  const ENGINE_STEP_CONTRACT = 2;
  const engine = deps.engine ?? productionEngineDeps();
  return interpretAutomation(
    { runId: input.runId, automationId: input.automationId, contract: ENGINE_STEP_CONTRACT },
    engine,
  );
}

export const automationRunWorkflow = DBOS.registerWorkflow(automationRunWorkflowImpl, {
  name: "AutomationRunWorkflow",
});
