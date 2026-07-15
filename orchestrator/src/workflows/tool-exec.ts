import { DBOS } from "@dbos-inc/dbos-sdk";

import {
  executeToolCall,
  productionToolExecDeps,
  submitToolExecution,
  type ToolExecInput,
} from "../tools/exec.ts";

/** Keep this registered body deliberately tiny and stable: all evolving tool
 * behavior lives in the named plain functions invoked by these durable steps. */
async function toolExecWorkflowEntry(input: ToolExecInput): Promise<void> {
  const outcome = await DBOS.runStep(
    () => executeToolCall(input, productionToolExecDeps),
    { name: "tool-exec-run" },
  );
  await DBOS.runStep(
    () => submitToolExecution(input, outcome, productionToolExecDeps),
    { name: "tool-exec-submit" },
  );
}

export const toolExecWorkflow = DBOS.registerWorkflow(toolExecWorkflowEntry, {
  name: "ToolExecWorkflow",
});
