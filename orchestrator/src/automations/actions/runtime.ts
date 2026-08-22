/** IntegrationActionRuntime over the catalog executor (ADR 0119 D5) — the
 * production implementation of the engine seam declared in engine/deps.ts.
 */

import type { IntegrationActionRuntime } from "../engine/deps.ts";
import {
  executeIntegrationAction,
  type ExecuteIntegrationActionDeps,
} from "./execute.ts";

export function makeIntegrationActionRuntime(
  deps: ExecuteIntegrationActionDeps = {},
): IntegrationActionRuntime {
  return {
    execute(input) {
      return executeIntegrationAction(
        {
          provider: input.provider,
          actionId: input.actionId,
          ...(input.connectionId !== undefined ? { connectionId: input.connectionId } : {}),
          params: input.params,
        },
        { runId: input.runId, stepPath: input.stepPath },
        deps,
      );
    },
  };
}
