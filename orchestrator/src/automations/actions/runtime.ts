/** IntegrationActionRuntime over the catalog executor (ADR 0119 D5) — the
 * production implementation of the engine seam declared in engine/deps.ts.
 *
 * ADR 0120: this is also THE handle-write site. After a successful
 * execution by an instance-bound run, every declared `handles` template
 * that renders (over `input.*` params and `output.*` mapped outputs) is
 * written to the instance ledger — in the same step, so a crash between
 * the provider call and the write replays into `already_ours`/`recorded`
 * and converges. A `conflict` (another workstream owns the handle) is a
 * typed permanent failure: routing is never silently rebound. Dry runs
 * never reach this module (the block stubs before the runtime).
 */

import type { IntegrationActionRuntime } from "../engine/deps.ts";
import type { AutomationInstanceStore } from "../../db/automation-instances.ts";
import { makeAutomationInstanceStore } from "../../db/automation-instances.ts";
import { canonicalHandle, renderHandleParts } from "../handles.ts";
import { IntegrationActionError } from "./errors.ts";
import {
  executeIntegrationAction,
  findAction,
  type ExecuteIntegrationActionDeps,
} from "./execute.ts";

export interface IntegrationActionRuntimeDeps extends ExecuteIntegrationActionDeps {
  /** ADR 0120: the handle ledger writer (lazy default; tests inject). */
  instances?: Pick<AutomationInstanceStore, "recordInstanceHandle">;
}

export function makeIntegrationActionRuntime(
  deps: IntegrationActionRuntimeDeps = {},
): IntegrationActionRuntime {
  let defaultInstances: AutomationInstanceStore | undefined;
  const instances = () => deps.instances ?? (defaultInstances ??= makeAutomationInstanceStore());
  return {
    async execute(input) {
      const outputs = await executeIntegrationAction(
        {
          provider: input.provider,
          actionId: input.actionId,
          ...(input.connectionId !== undefined ? { connectionId: input.connectionId } : {}),
          params: input.params,
        },
        { runId: input.runId, stepPath: input.stepPath },
        deps,
      );

      const instanceId = input.instanceId ?? "";
      const automationId = input.automationId ?? "";
      if (instanceId !== "" && automationId !== "") {
        const { action } = await findAction(input.provider, input.actionId, deps.connectors);
        for (const template of action.handles ?? []) {
          const rendered = renderHandleParts(template.parts, (path) => {
            const [scope, field] = path.split(".");
            if (scope === "input" && field !== undefined) return input.params[field];
            if (scope === "output" && field !== undefined) return outputs[field];
            return undefined;
          });
          if (rendered === null) continue;
          const handle = canonicalHandle(input.provider, rendered);
          const result = await instances().recordInstanceHandle({
            automationId,
            handle,
            instanceId,
            writtenBy: `${input.runId}:${input.stepPath}`,
          });
          if (result.kind === "conflict") {
            throw new IntegrationActionError(
              `handle "${handle}" already routes to workstream ${result.instanceId} — ` +
                "refusing to rebind (close or reuse that workstream instead)",
              true,
            );
          }
        }
      }
      return outputs;
    },
  };
}
