/**
 * Slack Events API endpoint (ADR 0059 P2.8).
 *
 *   POST /api/v1/integrations/slack/events
 *
 * Verifies every request with the Slack SDK's standalone `isValidSlackRequest`
 * (HMAC + staleness — no hand-rolled crypto), echoes the url_verification
 * challenge, and turns an `app_mention` into an idempotent
 * `startWorkflow(SlackThreadWorkflow) + send(mention)` — acking 200 only after
 * BOTH durable writes commit (Invariant 3); a crash before the ack is covered
 * by Slack's retry (the `event_id` idempotency key dedupes the send). The
 * workflow id carries the thread-reuse epoch (Invariant 4). No Slack API calls
 * here, so the handler is trivially under the 3s ack budget.
 */

import { Hono } from "hono";
import { DBOS } from "@dbos-inc/dbos-sdk";
import { isValidSlackRequest } from "@slack/bolt";

import { getSlackSigningSecret } from "../integrations/slack.ts";
import { classifySlackEvent } from "../integrations/slack-webhook.ts";
import { threadHash, selectThreadWorkflowId } from "../workflows/thread-workflow-id.ts";
import { slackThreadWorkflow } from "../workflows/slack-thread.ts";
import { THREAD_TOPIC, type ThreadInbox } from "../workflows/thread-inbox.ts";

/** DBOS statuses a workflow can't re-run from → the thread is done (Invariant 4). */
const TERMINAL_WF = new Set(["SUCCESS", "ERROR", "MAX_RECOVERY_ATTEMPTS_EXCEEDED", "CANCELLED"]);

export interface SlackEventsDeps {
  /** The Slack signing secret resolver (default = the org-secret resolver). */
  signingSecret?: () => Promise<string>;
}

export function makeSlackEventsRoute(deps: SlackEventsDeps = {}): Hono {
  const signingSecret = deps.signingSecret ?? getSlackSigningSecret;
  const app = new Hono();

  app.post("/api/v1/integrations/slack/events", async (c) => {
    const rawBody = await c.req.text();
    const valid = isValidSlackRequest({
      signingSecret: await signingSecret(),
      body: rawBody,
      headers: {
        "x-slack-signature": c.req.header("x-slack-signature") ?? "",
        "x-slack-request-timestamp": Number(c.req.header("x-slack-request-timestamp") ?? 0),
      },
    });
    if (!valid) return c.json({ error: "invalid signature" }, 401);

    const evt = classifySlackEvent(rawBody);
    if (evt.kind === "challenge") return c.text(evt.challenge);
    if (evt.kind === "ignore") return c.body(null, 200);

    const m = evt.mention;
    const workflowId = await selectThreadWorkflowId(
      threadHash(m.team, m.channel, m.threadRoot),
      async (id) => {
        const status = await DBOS.getWorkflowStatus(id);
        return status != null && TERMINAL_WF.has(status.status);
      },
    );
    // 1st mention creates the thread workflow; later ones are a no-op start and
    // the send delivers. Ack only after both commit (Invariant 3); the event_id
    // idempotency key makes a Slack retry a no-op.
    await DBOS.startWorkflow(slackThreadWorkflow, { workflowID: workflowId })();
    await DBOS.send<ThreadInbox>(workflowId, { kind: "trigger_mention", mention: m }, THREAD_TOPIC, m.eventId);
    return c.body(null, 200);
  });

  return app;
}

export default makeSlackEventsRoute();
