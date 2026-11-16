/** Linear's built-in automation actions. */

import { defineActions } from "../../automations/actions/builtin.ts";
import { actionClientId } from "../../automations/actions/client-id.ts";
import { IntegrationActionError } from "../../automations/actions/errors.ts";
import { optionalString, requireString } from "../../automations/actions/params.ts";
import { runIntegrationOp } from "../../integrations/run-op.ts";
import { LinearError, makeLinearIssueClient, type LinearIssueClient } from "./issues.ts";

export const LINEAR_ACTIONS = defineActions<LinearIssueClient>({
  client: (_provider, runOpDeps) =>
    makeLinearIssueClient({
      runIntegrationOp: (provider, request) => runIntegrationOp(provider, request, runOpDeps),
    }),
  actions: {
    // The caller mints the issue id, so a retried create adopts the issue
    // the first attempt made.
    create_issue: async (params, ctx, client) => {
      try {
        const issue = await client.createIssue({
          id: actionClientId(ctx.runId, ctx.stepPath),
          teamId: requireString(params, "teamId"),
          title: requireString(params, "title"),
          description: optionalString(params, "description") ?? "",
        });
        return { id: issue.id, identifier: issue.identifier, url: issue.url };
      } catch (error) {
        if (!(error instanceof LinearError)) throw error;
        throw new IntegrationActionError(error.message, !error.retryable, error.status);
      }
    },
  },
});
