import { z } from "zod";
import { waitFor } from "./bootstrap.ts";
import { requireThat, sha256, type Context } from "./context.ts";
import { canonicalJson } from "./guest.ts";
import type { Principals } from "./scenario-runtime.ts";

export type CreatedScenarioTask = {
  taskId: string;
  sessionId: string;
  automation?: {
    id: string;
    runId: string;
    runtimeActorId: string;
    definitionActorId: string;
    triggerActorId: string;
    definitionSha256: string;
  };
};
export async function createAutomationTask(
  ctx: Context,
  principals: Principals,
  profileId: string,
  suffix: string,
): Promise<CreatedScenarioTask> {
  const definition = {
    engine: 1,
    trigger: { kind: "manual" },
    blocks: [
      {
        id: "launch",
        type: "create_session",
        config: {
          profileId,
          promptTemplate: "",
          titleTemplate: "Fixture automation task",
          includeEventContext: false,
          keepOnFinish: true,
        },
      },
    ],
    inputsSchema: [],
    settings: { endSessionsOnFinish: false, runDeadlineSeconds: 120 },
  };
  const created = await principals.admin.automations.createAutomation({
    name: `Fixture ${suffix}`,
    enabled: true,
    definitionJson: JSON.stringify(definition),
    inputsJson: "{}",
  });
  requireThat(created.automation?.id, "public_automation_created");
  const automationId = created.automation.id;
  const launched = await principals.automation.automations.runNow({
    automationId,
  });
  requireThat(launched.runId, "public_automation_run");
  const observed = await waitFor(
    ctx,
    "automation_actual_launch",
    () =>
      principals.automation.automationRuns.getRun({ runId: launched.runId }),
    (value) => {
      requireThat(
        value.run?.brief &&
          !value.run.brief.dryRun &&
          value.run.brief.automationId === automationId &&
          value.run.brief.triggerSource === "manual",
        "actual_manual_automation_run",
      );
      requireThat(
        !["failed", "halted", "deadline", "superseded"].includes(
          value.run.brief.status,
        ),
        "automation_launch_failed",
      );
      return value.run.steps.some(
        (step) => step.blockId === "launch" && step.status === "succeeded",
      );
    },
  );
  const steps = observed.run!.steps.filter(
    (step) => step.blockId === "launch" && step.status === "succeeded",
  );
  requireThat(steps.length === 1, "single_automation_launch");
  const outputs = z
    .object({
      session_id: z.uuid(),
      task_id: z.string().min(1),
      initial_prompt: z.literal(false),
      prompt: z.literal(""),
    })
    .safeParse(JSON.parse(steps[0]!.outputsJson));
  requireThat(
    outputs.success &&
      observed.run!.sessionIds.length === 1 &&
      observed.run!.sessionIds[0] === outputs.data.session_id,
    "automation_public_session_identity",
  );
  const titled = await principals.admin.tasks.updateTask({
    taskId: outputs.data.task_id,
    title: "Fixture automation task",
  });
  requireThat(
    titled.task?.titleIsCustom === true,
    "automation_custom_title_before_prompt",
  );
  return {
    taskId: outputs.data.task_id,
    sessionId: outputs.data.session_id,
    automation: {
      id: automationId,
      runId: launched.runId,
      runtimeActorId: `automation:${automationId}`,
      definitionActorId: principals.actorIds.admin,
      triggerActorId: principals.actorIds.automation,
      definitionSha256: sha256(canonicalJson(definition)),
    },
  };
}
