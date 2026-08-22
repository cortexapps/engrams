/**
 * Slack events route — verification + challenge wiring (ADR 0060 P2.8).
 *
 * Exercises the route's use of the SDK verifier (isValidSlackRequest) and the
 * url_verification echo via Hono's in-memory request (no server, no DBOS — the
 * challenge path returns before any workflow call). The app_mention →
 * startWorkflow+send path is engine integration (covered later).
 */

import { expect, test, describe } from "bun:test";
import { createHmac } from "node:crypto";
import { makeSlackEventsRoute } from "../routes/slack-events.ts";
import { fakeIngress } from "./github-events-route.test.ts";
import type { SourceMention } from "../workflows/thread-inbox.ts";

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

/** The per-channel window (ADR 0119 phase 4.6): flagged channel × kill switch. */
describe("the Slack automation window", () => {
  function mentionBody(channel: string) {
    return JSON.stringify({
      type: "event_callback",
      team_id: "T1",
      event_id: `Ev-${channel}`,
      event: {
        type: "app_mention",
        channel,
        user: "U1",
        ts: "100.1",
        text: "<@UBOT> hello",
      },
    });
  }

  function windowed(options: { flagged: string[]; killSwitch: boolean }) {
    const legacy: SourceMention[] = [];
    const ingress = fakeIngress();
    const app = makeSlackEventsRoute({
      signingSecret: async () => SECRET,
      ingress: ingress.deps,
      channelOnAutomation: async (channel) => options.flagged.includes(channel),
      automationKillSwitch: () => options.killSwitch,
      startLegacyThread: async (m) => {
        legacy.push(m);
      },
    });
    return { app, legacy, ingress };
  }

  async function post(app: ReturnType<typeof makeSlackEventsRoute>, channel: string) {
    const body = mentionBody(channel);
    return app.request(PATH, { method: "POST", body, headers: signed(body) });
  }

  test("flagged channel, switch off → spine dispatches, legacy workflow skipped", async () => {
    const w = windowed({ flagged: ["C-on"], killSwitch: false });
    const res = await post(w.app, "C-on");
    expect(res.status).toBe(200);
    expect(w.ingress.dispatched.map((d) => d.scopeValue)).toEqual(["C-on"]);
    expect(w.legacy).toEqual([]);
  });

  test("unflagged channel, switch off → spine dispatches AND legacy runs (byte-identical legacy path)", async () => {
    const w = windowed({ flagged: ["C-on"], killSwitch: false });
    const res = await post(w.app, "C-off");
    expect(res.status).toBe(200);
    expect(w.ingress.dispatched.map((d) => d.scopeValue)).toEqual(["C-off"]);
    expect(w.legacy.map((m) => m.channel)).toEqual(["C-off"]);
  });

  test("flagged channel, switch ON → legacy runs (the brake wins over the flag)", async () => {
    const w = windowed({ flagged: ["C-on"], killSwitch: true });
    const res = await post(w.app, "C-on");
    expect(res.status).toBe(200);
    expect(w.legacy.map((m) => m.channel)).toEqual(["C-on"]);
  });

  test("unflagged channel, switch ON → legacy runs", async () => {
    const w = windowed({ flagged: [], killSwitch: true });
    await post(w.app, "C-off");
    expect(w.legacy.map((m) => m.channel)).toEqual(["C-off"]);
  });

  test("the flag is never consulted when the switch is on", async () => {
    let asked = 0;
    const app = makeSlackEventsRoute({
      signingSecret: async () => SECRET,
      ingress: fakeIngress().deps,
      channelOnAutomation: async () => {
        asked += 1;
        return true;
      },
      automationKillSwitch: () => true,
      startLegacyThread: async () => {},
    });
    await post(app, "C-on");
    expect(asked).toBe(0);
  });
});
