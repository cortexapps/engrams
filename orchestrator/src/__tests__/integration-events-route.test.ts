/** The shared provider event route: which providers it serves and how it
 * refuses a delivery it cannot verify. */

import { createHmac } from "node:crypto";
import { describe, expect, test } from "bun:test";

import { connectorRegistry } from "../connectors/registry.ts";
import type { RecordIntegrationEventInput } from "../db/integration-events.ts";
import { linearEventSource } from "../providers/linear/events.ts";
import { defaultEventSources, makeIntegrationEventsRoute } from "../routes/integration-events.ts";

const SECRET = "test-signing-secret";
const NOW = new Date("2026-08-31T17:06:35.000Z");
const BODY = JSON.stringify({ type: "Issue", action: "create", webhookTimestamp: NOW.getTime() });

function route(resolve: () => Promise<string>, recorded: RecordIntegrationEventInput[] = []) {
  return makeIntegrationEventsRoute({
    sources: [linearEventSource],
    secrets: { resolve },
    now: () => NOW,
    ingress: {
      connectionIdFor: async () => "conn-1",
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
        matched: 0, started: 0, joined: 0, queued: 0, skipped: 0, filtered: 0, dropped: 0,
        suppressed: [], failed: 0, builtins: {},
      }),
    },
  });
}

function post(app: ReturnType<typeof route>, provider: string, signingSecret = SECRET) {
  return app.request(`/api/v1/integrations/${provider}/events`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      "linear-delivery": "d-1",
      "linear-signature": createHmac("sha256", signingSecret).update(BODY).digest("hex"),
    },
    body: BODY,
  });
}

describe("provider event route", () => {
  test("the default sources are the connectors that declare a webhook ingress", () => {
    const declared = [...connectorRegistry().values()]
      .filter((connector) => connector.webhook?.ingress)
      .map((connector) => connector.provider);
    expect(defaultEventSources().map((source) => source.provider).sort()).toEqual(declared.sort());
  });

  test("a signed delivery reaches the ledger", async () => {
    const recorded: RecordIntegrationEventInput[] = [];
    const res = await post(route(async () => SECRET, recorded), "linear");
    expect(res.status).toBe(200);
    expect(recorded).toHaveLength(1);
  });

  test("a provider with no event source is not found", async () => {
    const recorded: RecordIntegrationEventInput[] = [];
    const res = await post(route(async () => SECRET, recorded), "github");
    expect(res.status).toBe(404);
    expect(recorded).toHaveLength(0);
  });

  test("a delivery signed with another secret is refused", async () => {
    const recorded: RecordIntegrationEventInput[] = [];
    const res = await post(route(async () => SECRET, recorded), "linear", "wrong-secret");
    expect(res.status).toBe(401);
    expect(recorded).toHaveLength(0);
  });

  test("an unreadable signing secret is a refusal, not a server fault", async () => {
    const recorded: RecordIntegrationEventInput[] = [];
    const app = route(async () => {
      throw new Error("secret not sealed");
    }, recorded);
    const res = await post(app, "linear");
    expect(res.status).toBe(401);
    expect(recorded).toHaveLength(0);
  });
});
