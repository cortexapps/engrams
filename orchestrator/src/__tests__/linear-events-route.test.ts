/**
 * Linear ingress: the team scope an entrypoint filters on.
 *
 * Linear puts the team in a different place per entity. An Issue payload
 * carries `data.team`; a Comment has no team on `data` at all and nests it
 * under the issue. Reading only `data.team.key` ledgered every comment with NO
 * scope, and `triggerMatches` drops a scoped trigger whose event has no
 * scopeValue — so a team-scoped automation could never see a comment.
 */

import { createHmac } from "node:crypto";
import { describe, expect, test } from "bun:test";

import { makeLinearEventsRoute } from "../routes/linear-events.ts";
import type { RecordIntegrationEventInput } from "../db/integration-events.ts";

const SECRET = "linear-test-signing-secret";
const NOW = new Date("2026-08-31T17:06:35.000Z");

/** Drive one signed delivery and return what reached the ledger. */
async function deliver(payload: Record<string, unknown>): Promise<RecordIntegrationEventInput> {
  const recorded: RecordIntegrationEventInput[] = [];
  const body = JSON.stringify({ ...payload, webhookTimestamp: NOW.getTime() });
  const app = makeLinearEventsRoute({
    secrets: { resolve: async () => SECRET },
    now: () => NOW,
    ingress: {
      now: () => NOW,
      connectionIdFor: async () => "conn-linear",
      store: {
        record: async (input) => {
          recorded.push(input);
          return { recorded: true, id: "evt-1" };
        },
        sweep: async () => 0,
        list: async () => [],
        getLatest: async () => null,
        listObservedEventKeys: async () => [],
        getById: async () => null,
        listObservedScopeValues: async () => [],
      },
      dispatch: async () => ({
        matched: 0, started: 0, joined: 0, queued: 0, skipped: 0, dropped: 0,
        suppressed: [], failed: 0, builtins: {},
      }),
    },
  });

  const res = await app.request("/api/v1/integrations/linear/events", {
    method: "POST",
    headers: {
      "content-type": "application/json",
      "linear-delivery": `d-${Math.random().toString(36).slice(2)}`,
      "linear-signature": createHmac("sha256", SECRET).update(body).digest("hex"),
    },
    body,
  });
  expect(res.status).toBe(200);
  expect(recorded).toHaveLength(1);
  return recorded[0]!;
}

describe("linear ingress: team scope", () => {
  test("an issue payload scopes on data.team.key", async () => {
    const got = await deliver({
      type: "Issue",
      action: "update",
      data: { id: "i1", identifier: "CD-495", team: { key: "CD", name: "Customer Delight" } },
    });
    expect(got.eventKey).toBe("issue.update");
    expect(got.scopeValue).toBe("CD");
  });

  test("a comment payload scopes on the ISSUE's team (the regression)", async () => {
    // Shape verified against a live CD-495 delivery: `data.team` is absent and
    // the team lives at `data.issue.team`.
    const got = await deliver({
      type: "Comment",
      action: "create",
      data: {
        id: "c1",
        body: "another comment",
        issue: { id: "i1", identifier: "CD-495", team: { key: "CD", name: "Customer Delight" } },
      },
    });
    expect(got.eventKey).toBe("comment.create");
    expect(got.scopeValue).toBe("CD");
  });

  test("data.team wins over the issue's team when both are present", async () => {
    const got = await deliver({
      type: "Comment",
      action: "remove",
      data: { id: "c1", team: { key: "CD" }, issue: { team: { key: "ENG" } } },
    });
    expect(got.scopeValue).toBe("CD");
  });

  test("a payload with no team anywhere ledgers with no scope", async () => {
    // Still ledgered — only a SCOPED trigger declines it; an unscoped one fires.
    const got = await deliver({ type: "Project", action: "update", data: { id: "p1" } });
    expect(got.eventKey).toBe("project.update");
    expect(got.scopeValue).toBeUndefined();
  });
});
