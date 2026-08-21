/** Ingress spine + provider routes (ADR 0119 D5) — in-memory seams, no DB.
 *
 * The spine's contract: bounded body → verify → parse (tolerant) → extract →
 * redact → ledger → dispatch seam, with provider handshakes and rejects
 * passed through. The Linear route is exercised end to end here because it
 * has no legacy path; GitHub/Slack legacy coexistence is covered in their
 * route suites.
 */

import { createHmac } from "node:crypto";
import { describe, expect, test } from "bun:test";

import {
  handleIntegrationDelivery,
  type IntegrationEventDispatchInput,
  type IntegrationEventRoute,
} from "../automations/integration-ingress.ts";
import type {
  IntegrationEventStore,
  RecordIntegrationEventInput,
} from "../db/integration-events.ts";
import { makeLinearEventsRoute } from "../routes/linear-events.ts";
import { makeSlackEventsRoute } from "../routes/slack-events.ts";

function fakeStore() {
  const recorded: RecordIntegrationEventInput[] = [];
  const store: IntegrationEventStore = {
    async record(input) {
      const duplicate = recorded.some(
        (e) =>
          e.provider === input.provider &&
          e.connectionId === input.connectionId &&
          e.deliveryId === input.deliveryId,
      );
      if (!duplicate) recorded.push(input);
      return { recorded: !duplicate };
    },
    async sweepExpired() {
      return 0;
    },
    async list() {
      return [];
    },
    async getLatest() {
      return null;
    },
    async listObservedEventKeys() {
      return [];
    },
  };
  return { recorded, store };
}

function seams() {
  const { recorded, store } = fakeStore();
  const dispatched: IntegrationEventDispatchInput[] = [];
  return {
    recorded,
    dispatched,
    deps: {
      store,
      connectionIdFor: async (provider: string) => `conn-${provider}`,
      dispatch: async (input: IntegrationEventDispatchInput) => {
        dispatched.push(input);
      },
    },
  };
}

const passRoute: IntegrationEventRoute = {
  provider: "generic",
  displayName: "Generic (default)",
  verify: async () => true,
  extract: (_headers, payload) => ({
    kind: "event",
    event: {
      eventKey: String(payload["key"] ?? "k"),
      deliveryId: String(payload["id"] ?? "d1"),
    },
  }),
};

function request(body: string, headers: Record<string, string> = {}): Request {
  return new Request("http://localhost/x", {
    method: "POST",
    body,
    headers: { "content-type": "application/json", ...headers },
  });
}

describe("handleIntegrationDelivery", () => {
  test("verifies, redacts, ledgers, and dispatches a delivery", async () => {
    const s = seams();
    const result = await handleIntegrationDelivery(
      passRoute,
      request(JSON.stringify({ key: "issue.create", id: "d-1", token: "sekret", title: "ok" })),
      s.deps,
    );
    expect(result.kind).toBe("recorded");
    expect(s.recorded).toHaveLength(1);
    // Redaction ran before persistence: secret-shaped keys never land.
    expect(s.recorded[0]!.payload["token"]).toBeUndefined();
    expect(s.recorded[0]!.payload["title"]).toBe("ok");
    expect(s.dispatched).toEqual([
      expect.objectContaining({
        provider: "generic",
        connectionId: "conn-generic",
        eventKey: "issue.create",
        deliveryId: "d-1",
      }),
    ]);
  });

  test("a duplicate delivery ledgers once but still reaches dispatch idempotently", async () => {
    const s = seams();
    const body = JSON.stringify({ key: "k", id: "dup" });
    await handleIntegrationDelivery(passRoute, request(body), s.deps);
    const second = await handleIntegrationDelivery(passRoute, request(body), s.deps);
    expect(second.kind).toBe("recorded");
    expect(second.kind === "recorded" && second.recorded).toBe(false);
    expect(s.recorded).toHaveLength(1);
  });

  test("an unverified delivery is rejected before any parsing", async () => {
    const s = seams();
    const result = await handleIntegrationDelivery(
      { ...passRoute, verify: async () => false },
      request("{"),
      s.deps,
    );
    expect(result.kind).toBe("rejected");
    expect(result.kind === "rejected" && result.response.status).toBe(401);
    expect(s.recorded).toHaveLength(0);
  });

  test("a signed but unparseable body skips tolerantly with the raw bytes", async () => {
    const s = seams();
    const result = await handleIntegrationDelivery(passRoute, request("not json"), s.deps);
    expect(result.kind).toBe("skipped");
    expect(result.kind === "skipped" && new TextDecoder().decode(result.rawBody)).toBe("not json");
    expect(s.recorded).toHaveLength(0);
  });

  test("a dispatch outage never fails the delivery (ledger row is durable)", async () => {
    const { store, recorded } = fakeStore();
    const result = await handleIntegrationDelivery(
      passRoute,
      request(JSON.stringify({ key: "k", id: "d9" })),
      {
        store,
        connectionIdFor: async () => "conn",
        dispatch: async () => {
          throw new Error("dispatch down");
        },
      },
    );
    expect(result.kind).toBe("recorded");
    expect(recorded).toHaveLength(1);
  });
});

// ---------------------------------------------------------------------------
// Linear route
// ---------------------------------------------------------------------------

const LINEAR_SECRET = "linear-signing";
const LINEAR_PATH = "/api/v1/integrations/linear/events";

function linearBody(overrides: Record<string, unknown> = {}): string {
  return JSON.stringify({
    type: "Issue",
    action: "create",
    webhookTimestamp: Date.now(),
    data: { id: "i1", identifier: "ENG-1", title: "t", team: { key: "ENG" } },
    ...overrides,
  });
}

function linearHeaders(body: string, overrides: Record<string, string> = {}) {
  return {
    "content-type": "application/json",
    "linear-signature": createHmac("sha256", LINEAR_SECRET).update(body).digest("hex"),
    "linear-delivery": "9f0c1c2e-7d34-4a89-b1de-000000000001",
    ...overrides,
  };
}

describe("POST /api/v1/integrations/linear/events", () => {
  function linearApp() {
    const s = seams();
    const app = makeLinearEventsRoute({
      secrets: { resolve: async () => LINEAR_SECRET },
      ingress: s.deps,
    });
    return { ...s, app };
  }

  test("a valid delivery ledgers with the team scope and dispatches", async () => {
    const { app, recorded, dispatched } = linearApp();
    const body = linearBody();
    const res = await app.request(LINEAR_PATH, {
      method: "POST",
      body,
      headers: linearHeaders(body),
    });
    expect(res.status).toBe(200);
    expect(recorded).toEqual([
      expect.objectContaining({
        provider: "linear",
        eventKey: "issue.create",
        deliveryId: "9f0c1c2e-7d34-4a89-b1de-000000000001",
        scopeValue: "ENG",
      }),
    ]);
    expect(dispatched).toHaveLength(1);
  });

  test("an invalid signature is 401", async () => {
    const { app, recorded } = linearApp();
    const body = linearBody();
    const res = await app.request(LINEAR_PATH, {
      method: "POST",
      body,
      headers: linearHeaders(body, { "linear-signature": "00".repeat(32) }),
    });
    expect(res.status).toBe(401);
    expect(recorded).toHaveLength(0);
  });

  test("a stale webhookTimestamp is 401 (replay guard)", async () => {
    const { app, recorded } = linearApp();
    const body = linearBody({ webhookTimestamp: Date.now() - 5 * 60_000 });
    const res = await app.request(LINEAR_PATH, {
      method: "POST",
      body,
      headers: linearHeaders(body),
    });
    expect(res.status).toBe(401);
    expect(recorded).toHaveLength(0);
  });

  test("a missing Linear-Delivery is 400", async () => {
    const { app } = linearApp();
    const body = linearBody();
    const headers = linearHeaders(body);
    // Build headers without the delivery id.
    const res = await app.request(LINEAR_PATH, {
      method: "POST",
      body,
      headers: { "content-type": headers["content-type"]!, "linear-signature": headers["linear-signature"]! },
    });
    expect(res.status).toBe(400);
  });

  test("a redelivery ledgers exactly once", async () => {
    const { app, recorded } = linearApp();
    const body = linearBody();
    const headers = linearHeaders(body);
    await app.request(LINEAR_PATH, { method: "POST", body, headers });
    await app.request(LINEAR_PATH, { method: "POST", body, headers });
    expect(recorded).toHaveLength(1);
  });
});

// ---------------------------------------------------------------------------
// Slack route: ledger + legacy coexistence for eligible events
// ---------------------------------------------------------------------------

const SLACK_SECRET = "slack-signing";
const SLACK_PATH = "/api/v1/integrations/slack/events";

function slackSigned(body: string) {
  const ts = String(Math.floor(Date.now() / 1000));
  const sig = "v0=" + createHmac("sha256", SLACK_SECRET).update(`v0:${ts}:${body}`).digest("hex");
  return {
    "x-slack-signature": sig,
    "x-slack-request-timestamp": ts,
    "content-type": "application/json",
  };
}

describe("slack events through the spine", () => {
  function slackApp() {
    const s = seams();
    const app = makeSlackEventsRoute({
      signingSecret: async () => SLACK_SECRET,
      ingress: s.deps,
    });
    return { ...s, app };
  }

  test("a plain channel message ledgers with the channel scope (no legacy workflow)", async () => {
    const { app, recorded } = slackApp();
    const body = JSON.stringify({
      type: "event_callback",
      event_id: "Ev123",
      event: { type: "message", text: "hi", user: "U1", channel: "C42", ts: "1.2" },
    });
    const res = await app.request(SLACK_PATH, { method: "POST", body, headers: slackSigned(body) });
    expect(res.status).toBe(200);
    expect(recorded).toEqual([
      expect.objectContaining({
        provider: "slack",
        eventKey: "message",
        deliveryId: "Ev123",
        scopeValue: "C42",
      }),
    ]);
  });

  test("a bot/edited message subtype never ledgers", async () => {
    const { app, recorded } = slackApp();
    const body = JSON.stringify({
      type: "event_callback",
      event_id: "Ev124",
      event: { type: "message", subtype: "message_changed", channel: "C42" },
    });
    const res = await app.request(SLACK_PATH, { method: "POST", body, headers: slackSigned(body) });
    expect(res.status).toBe(200);
    expect(recorded).toHaveLength(0);
  });

  test("a Slack retry with the same event_id ledgers once", async () => {
    const { app, recorded } = slackApp();
    const body = JSON.stringify({
      type: "event_callback",
      event_id: "Ev125",
      event: { type: "reaction_added", user: "U1", reaction: "+1", item: { channel: "C9", ts: "1.3" } },
    });
    await app.request(SLACK_PATH, { method: "POST", body, headers: slackSigned(body) });
    await app.request(SLACK_PATH, {
      method: "POST",
      body,
      headers: { ...slackSigned(body), "x-slack-retry-num": "1" },
    });
    expect(recorded).toHaveLength(1);
    expect(recorded[0]!.scopeValue).toBe("C9");
  });

  test("url_verification still echoes through the spine", async () => {
    const { app, recorded } = slackApp();
    const body = JSON.stringify({ type: "url_verification", challenge: "c-2" });
    const res = await app.request(SLACK_PATH, { method: "POST", body, headers: slackSigned(body) });
    expect(res.status).toBe(200);
    expect(await res.text()).toBe("c-2");
    expect(recorded).toHaveLength(0);
  });
});
