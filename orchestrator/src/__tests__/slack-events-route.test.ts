/**
 * Slack events route — verification + challenge wiring (ADR 0059 P2.8).
 *
 * Exercises the route's use of the SDK verifier (isValidSlackRequest) and the
 * url_verification echo via Hono's in-memory request (no server, no DBOS — the
 * challenge path returns before any workflow call). The app_mention →
 * startWorkflow+send path is engine integration (covered later).
 */

import { expect, test, describe } from "bun:test";
import { createHmac } from "node:crypto";
import { makeSlackEventsRoute } from "../routes/slack-events.ts";

const SECRET = "test-signing-secret";
const PATH = "/api/v1/integrations/slack/events";
const app = makeSlackEventsRoute({ signingSecret: async () => SECRET });

function signed(body: string, ts = String(Math.floor(Date.now() / 1000))) {
  const sig = "v0=" + createHmac("sha256", SECRET).update(`v0:${ts}:${body}`).digest("hex");
  return { "x-slack-signature": sig, "x-slack-request-timestamp": ts, "content-type": "application/json" };
}

describe("POST /api/v1/integrations/slack/events", () => {
  test("rejects an invalid signature with 401", async () => {
    const body = JSON.stringify({ type: "url_verification", challenge: "c1" });
    const res = await app.request(PATH, {
      method: "POST",
      body,
      headers: { ...signed(body), "x-slack-signature": "v0=deadbeef" },
    });
    expect(res.status).toBe(401);
  });

  test("echoes the url_verification challenge on a valid signature", async () => {
    const body = JSON.stringify({ type: "url_verification", challenge: "c1" });
    const res = await app.request(PATH, { method: "POST", body, headers: signed(body) });
    expect(res.status).toBe(200);
    expect(await res.text()).toBe("c1");
  });

  test("a stale timestamp fails verification (SDK)", async () => {
    const body = JSON.stringify({ type: "url_verification", challenge: "c1" });
    const stale = String(Math.floor(Date.now() / 1000) - 10 * 60);
    const res = await app.request(PATH, { method: "POST", body, headers: signed(body, stale) });
    expect(res.status).toBe(401);
  });
});
