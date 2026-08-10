/** Durable one-occurrence automation launch workflow (ADR 0102). */

import { DBOS } from "@dbos-inc/dbos-sdk";

import { buildAutomationTemplateContext, AutomationTemplateError, renderAutomationAction } from "../automations/template.ts";
import { makeWebhookAliasResolver } from "../automations/aliases.ts";
import { sessions as defaultSessions, images as defaultImages, harnessCatalog as defaultHarnessCatalog } from "../control-plane/client.ts";
import { makeAutomationStore, type AutomationWorkflowStore } from "../db/automations.ts";
import { getDb } from "../db/client.ts";
import { makeConnectorStore } from "../db/connectors.ts";
import { makeProfileStore } from "../db/profiles.ts";
import type { AutomationRunTrigger } from "../db/schema.ts";
import {
  createSessionForExistingTask,
  truncatePrompt,
  type CreateSessionForExistingTaskParams,
  type HarnessCatalogClient,
} from "../rpc/task-create.ts";
import type { ImagesClient } from "../rpc/profiles.ts";

export interface AutomationRunWorkflowInput {
  automationId: string;
  runId: string;
  trigger: AutomationRunTrigger;
  /** ISO timestamp for the occurrence; required for cron runs. */
  scheduledFor?: string;
  /** ISO timestamp at which the trigger was accepted by the orchestrator. */
  receivedAt: string;
}

export interface PreparedAutomationRun {
  profileId: string;
  prompt: string;
  title: string | null;
  /** ADR 0107: session mode for the initial prompt (e.g. "plan"). */
  harnessMode?: string;
  /** ADR 0063 B2: the automation's harness/model/effort override, if any.
   *  Resolved from the stored action so a retried occurrence launches the same
   *  harness the render step read. */
  harness?: string;
  model?: string;
  effort?: string;
}

export interface AutomationTaskCreator {
  create(input: AutomationRunWorkflowInput, prepared: PreparedAutomationRun): Promise<{
    taskId: string;
    sessionId: string;
  }>;
}

export interface AutomationStepOptions {
  retry: boolean;
  shouldRetry?: (error: unknown) => boolean | Promise<boolean>;
}

export type AutomationStepRunner = <T>(
  fn: () => Promise<T>,
  name: string,
  options: AutomationStepOptions,
) => Promise<T>;

export interface AutomationRunWorkflowDeps {
  store?: AutomationWorkflowStore;
  taskCreator?: AutomationTaskCreator;
  render?: typeof renderAutomationAction;
  step?: AutomationStepRunner;
  aliases?: (registrationId: string) => Promise<ReadonlyArray<{ path: string; alias: string }>>;
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function scheduledDate(input: AutomationRunWorkflowInput): Date | null {
  if (input.scheduledFor === undefined) return null;
  const date = new Date(input.scheduledFor);
  if (Number.isNaN(date.getTime())) {
    throw new Error(`invalid scheduledFor timestamp ${JSON.stringify(input.scheduledFor)}`);
  }
  return date;
}

function triggerSource(input: AutomationRunWorkflowInput): Record<string, unknown> {
  return {
    kind: input.trigger.source,
    ...(input.scheduledFor !== undefined ? { scheduledFor: input.scheduledFor } : {}),
    ...(input.trigger.eventKey !== undefined ? { eventKey: input.trigger.eventKey } : {}),
    ...(input.trigger.deliveryId !== undefined ? { deliveryId: input.trigger.deliveryId } : {}),
  };
}

type CreateExistingSession = (
  params: CreateSessionForExistingTaskParams,
) => Promise<{ sessionId: string }>;

export interface AutomationTaskCreatorDeps {
  store?: AutomationWorkflowStore;
  createSessionForExistingTask?: CreateExistingSession;
}

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

/** Build the task/session primitive separately from the workflow graph so unit
 * tests can prove render failures never cross the billable-launch boundary. */
export function makeAutomationTaskCreator(
  deps: AutomationTaskCreatorDeps = {},
): AutomationTaskCreator {
  let resolvedStore = deps.store;
  const store = () => (resolvedStore ??= makeAutomationStore());
  let createExistingSession = deps.createSessionForExistingTask;
  const createSession = (): CreateExistingSession => {
    if (createExistingSession) return createExistingSession;
    const database = getDb();
    createExistingSession = (params) => createSessionForExistingTask(
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
    async create(input, prepared) {
      const taskId = await store().ensureAutomationTask({
        runId: input.runId,
        automationId: input.automationId,
        title: prepared.title ?? truncatePrompt(prepared.prompt),
        source: {
          provider: "automation",
          automationId: input.automationId,
          runId: input.runId,
          trigger: triggerSource(input),
        },
      });
      const existingSessionId = await store().getAutomationTaskSession(input.runId);
      if (existingSessionId !== null) return { taskId, sessionId: existingSessionId };

      const { sessionId } = await createSession()({
        taskId,
        taskType: "automation",
        profileId: prepared.profileId,
        integrationPrincipalId: `automation:${input.automationId}`,
        role: "primary",
        prompt: prepared.prompt,
        ...(prepared.harnessMode != null ? { harnessMode: prepared.harnessMode } : {}),
        // ADR 0063 B2: the automation's harness/model/effort selection, absent
        // when it inherits the profile's default.
        ...(prepared.harness !== undefined ? { harness: prepared.harness } : {}),
        ...(prepared.model !== undefined ? { model: prepared.model } : {}),
        ...(prepared.effort !== undefined ? { effort: prepared.effort } : {}),
        registerListener: true,
        // No owner and no policy overrides: the shared compiler keeps profile
        // secrets, env, capabilities, and network policy intact.
      });
      return { taskId, sessionId };
    },
  };
}

export async function automationRunWorkflowImpl(
  input: AutomationRunWorkflowInput,
  deps: AutomationRunWorkflowDeps = {},
): Promise<void> {
  const store = deps.store ?? makeAutomationStore();
  const taskCreator = deps.taskCreator ?? makeAutomationTaskCreator({ store });
  const render = deps.render ?? renderAutomationAction;
  const aliases = deps.aliases ?? makeWebhookAliasResolver();
  const step: AutomationStepRunner = deps.step ?? ((fn, name, options) =>
    DBOS.runStep(fn, {
      name,
      retriesAllowed: options.retry,
      ...(options.shouldRetry ? { shouldRetry: options.shouldRetry } : {}),
    }));

  const run = await step(
    () => store.ensureRun({
      id: input.runId,
      automationId: input.automationId,
      trigger: input.trigger,
      scheduledFor: scheduledDate(input),
    }),
    "ensureAutomationRun",
    { retry: true },
  );
  // DBOS normally prevents this body from re-running. Keep the ledger itself
  // defensive too: a terminal occurrence can never create another task.
  if (run.status !== "pending") return;

  let prepared: PreparedAutomationRun;
  try {
    prepared = await step(
      async () => {
        const automation = await store.getAutomation(input.automationId);
        if (!automation) throw new Error(`automation ${input.automationId} not found`);
        const scheduledFor = input.scheduledFor;
        if (input.trigger.source === "cron" && scheduledFor === undefined) {
          throw new Error("cron automation run is missing scheduledFor");
        }
        const webhookAliases = automation.trigger.kind === "webhook"
          ? await aliases(automation.trigger.registrationId)
          : [];
        const context = buildAutomationTemplateContext({
          automationName: automation.name,
          triggerKind: input.trigger.source,
          receivedAt: input.receivedAt,
          ...(input.trigger.eventKey !== undefined ? { eventKey: input.trigger.eventKey } : {}),
          ...(scheduledFor !== undefined ? { scheduledFor } : {}),
          ...(input.trigger.payload !== undefined ? { rawPayload: input.trigger.payload } : {}),
          aliases: webhookAliases,
        });
        const rendered = await render(automation.action, context);
        const title = rendered.title ?? null;
        await store.recordRendered(input.runId, rendered.prompt, title);
        return {
          profileId: automation.action.profileId,
          prompt: rendered.prompt,
          title,
          ...(automation.action.harnessMode != null
            ? { harnessMode: automation.action.harnessMode }
            : {}),
          ...(automation.action.harness !== undefined
            ? { harness: automation.action.harness }
            : {}),
          ...(automation.action.model !== undefined ? { model: automation.action.model } : {}),
          ...(automation.action.effort !== undefined ? { effort: automation.action.effort } : {}),
        };
      },
      "renderAutomationAction",
      {
        retry: true,
        shouldRetry: (error) => !(error instanceof AutomationTemplateError),
      },
    );
  } catch (error) {
    if (error instanceof AutomationTemplateError) {
      await step(
        () => store.markRunRenderFailed(input.runId, messageOf(error)),
        "markAutomationRenderFailed",
        { retry: true },
      );
    } else {
      await step(
        () => store.markRunLaunchFailed(input.runId, messageOf(error)),
        "markAutomationLaunchFailed",
        { retry: true },
      );
    }
    return;
  }

  let launched: { taskId: string; sessionId: string };
  try {
    launched = await step(
      () => taskCreator.create(input, prepared),
      "launchAutomationTask",
      { retry: true },
    );
  } catch (error) {
    await step(
      () => store.markRunLaunchFailed(input.runId, messageOf(error)),
      "markAutomationLaunchFailed",
      { retry: true },
    );
    return;
  }

  await step(
    () => store.markRunLaunched(input.runId, launched.taskId, launched.sessionId),
    "markAutomationLaunched",
    { retry: true },
  );
}

export const automationRunWorkflow = DBOS.registerWorkflow(automationRunWorkflowImpl, {
  name: "AutomationRunWorkflow",
});
