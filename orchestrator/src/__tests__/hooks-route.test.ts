import { createHmac } from "node:crypto";
import { describe, expect, test } from "bun:test";

import type { DispatchWebhookInput } from "../automations/dispatch.ts";
import type { WebhookRegistrationRow } from "../db/automations.ts";
import { makeHooksRoute } from "../routes/hooks.ts";

const SECRET = "generic-hook-secret";
const PATH = "/api/v1/hooks/my-hook";
const NOW = new Date("2026-07-22T12:00:00Z");
const registration: WebhookRegistrationRow = {
  id: "my-hook",
  name: "My hook",
  verification: {
    scheme: "generic_hmac_sha256",
    secretRef: "webhook.my-hook.secret",
  },
  providerHint: null,
  createdByUserId: "admin-1",
  createdAt: NOW,
  updatedAt: NOW,
};

function signedHeaders(body: string): Record<string, string> {
  return {
    "content-type": "application/json",
    "x-engrams-event": "incident.opened",
    "x-engrams-delivery": "delivery-1",
    "x-engrams-signature-256": `sha256=${createHmac("sha256", SECRET).update(body).digest("hex")}`,
  };
}

function fixture(found = true) {
  const dispatches: DispatchWebhookInput[] = [];
  const secretRequests: Array<{ provider: string; secretRef: string }> = [];
  return {
    dispatches,
    secretRequests,
    app: makeHooksRoute({
      store: { getRegistration: async () => found ? registration : null },
      secretResolver: {
        async resolve(input) {
          secretRequests.push(input);
          return SECRET;
        },
      },
      async dispatch(input) {
        dispatches.push(input);
      },
      now: () => NOW,
    }),
  };
}

describe("POST /api/v1/hooks/:registrationId", () => {
  test("returns 404 for an unknown registration before reading or resolving", async () => {
    const f = fixture(false);
    const res = await f.app.request(PATH, { method: "POST", body: "not json" });
    expect(res.status).toBe(404);
    expect(f.secretRequests).toEqual([]);
  });

  test("returns 404 for an invalid registration slug before lookup side effects", async () => {
    const f = fixture();
    const res = await f.app.request("/api/v1/hooks/Bad_Slug", {
      method: "POST",
      body: "ignored",
    });
    expect(res.status).toBe(404);
    expect(f.secretRequests).toEqual([]);
  });

  test("rejects an invalid signature before parsing or dispatch", async () => {
    const f = fixture();
    const res = await f.app.request(PATH, {
      method: "POST",
      body: "not json",
      headers: { "x-engrams-signature-256": "sha256=bad" },
    });
    expect(res.status).toBe(401);
    expect(f.dispatches).toEqual([]);
  });

  test("rejects an oversized actual stream despite an understated length", async () => {
    const f = fixture();
    const res = await f.app.request(PATH, {
      method: "POST",
      body: new Uint8Array(2 * 1024 * 1024 + 1),
      headers: { "content-length": "1" },
    });
    expect(res.status).toBe(413);
    expect(f.secretRequests).toEqual([]);
    expect(f.dispatches).toEqual([]);
  });

  test("rejects a verified malformed event header", async () => {
    const body = JSON.stringify({ ok: true });
    const f = fixture();
    const res = await f.app.request(PATH, {
      method: "POST",
      body,
      headers: { ...signedHeaders(body), "x-engrams-event": "Bad Event" },
    });
    expect(res.status).toBe(400);
    expect(f.dispatches).toEqual([]);
  });

  test("rejects a correctly signed non-JSON body after verification", async () => {
    const body = "not json";
    const f = fixture();
    const res = await f.app.request(PATH, {
      method: "POST",
      body,
      headers: signedHeaders(body),
    });
    expect(res.status).toBe(400);
    expect(f.secretRequests).toHaveLength(1);
    expect(f.dispatches).toEqual([]);
  });

  test("verifies, redacts, dispatches, and acknowledges a normal delivery", async () => {
    const body = JSON.stringify({
      incident: { id: 7, authorization: "Bearer secret" },
      access_token: "top-secret",
    });
    const f = fixture();
    const res = await f.app.request(PATH, {
      method: "POST",
      body,
      headers: signedHeaders(body),
    });
    expect(res.status).toBe(200);
    expect(f.secretRequests).toEqual([{
      provider: "webhook",
      secretRef: "webhook.my-hook.secret",
    }]);
    expect(f.dispatches).toEqual([{
      registrationId: "my-hook",
      registration,
      eventKey: "incident.opened",
      deliveryId: "delivery-1",
      payload: { incident: { id: 7 } },
      receivedAt: NOW,
    }]);
  });
});
