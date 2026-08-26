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
import type { DispatchIntegrationResult } from "../automations/dispatch.ts";

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

/** The per-channel window (ADR 0119 phase 4.6): the route skips the legacy
 * workflow iff the dispatcher handed the delivery to the Slack built-in.
 * There is no separate flag lookup (and no per-pod cache): the decision is
 * the same admission result the engine acted on, so two replicas can never
 * disagree on who owns a delivery. */
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

  /** A dispatcher that admits the Slack built-in for `flagged` channels with
   * the given outcome (the kill switch = the dispatcher refusing the
   * built-in, i.e. it is simply absent from `builtins`). */
  function windowed(options: {
    flagged: string[];
    outcome?: "started" | "joined" | "queued" | "skipped";
  }) {
    const legacy: SourceMention[] = [];
    const ingress = fakeIngress();
    const dispatched = ingress.dispatched;
    const app = makeSlackEventsRoute({
      signingSecret: async () => SECRET,
      ingress: {
        ...ingress.deps,
        dispatch: async (input): Promise<DispatchIntegrationResult> => {
          dispatched.push(input);
          const on = input.scopeValue !== undefined && options.flagged.includes(input.scopeValue);
          return {
            matched: on ? 1 : 0,
            started: on ? 1 : 0,
            joined: 0,
            queued: 0,
            skipped: 0,
            dropped: 0,
            suppressed: [],
            failed: 0,
            builtins: on ? { slack_brain: options.outcome ?? "started" } : {},
          };
        },
      },
      startLegacyThread: async (m) => {
        legacy.push(m);
      },
    });
    return { app, legacy, dispatched };
  }

  async function post(app: ReturnType<typeof makeSlackEventsRoute>, channel: string) {
    const body = mentionBody(channel);
    return app.request(PATH, { method: "POST", body, headers: signed(body) });
  }

  test("the dispatcher started a run for the built-in → legacy workflow skipped", async () => {
    const w = windowed({ flagged: ["C-on"] });
    const res = await post(w.app, "C-on");
    expect(res.status).toBe(200);
    expect(w.dispatched.map((d) => d.scopeValue)).toEqual(["C-on"]);
    expect(w.legacy).toEqual([]);
  });

  test("a follow-up the dispatcher JOINED into the active run → legacy workflow skipped", async () => {
    const w = windowed({ flagged: ["C-on"], outcome: "joined" });
    await post(w.app, "C-on");
    expect(w.legacy).toEqual([]);
  });

  test("unflagged channel → spine dispatches AND legacy runs (byte-identical legacy path)", async () => {
    const w = windowed({ flagged: ["C-on"] });
    const res = await post(w.app, "C-off");
    expect(res.status).toBe(200);
    expect(w.dispatched.map((d) => d.scopeValue)).toEqual(["C-off"]);
    expect(w.legacy.map((m) => m.channel)).toEqual(["C-off"]);
  });

  test("the built-in was matched but its admission was skipped → legacy runs (no silent thread)", async () => {
    const w = windowed({ flagged: ["C-on"], outcome: "skipped" });
    await post(w.app, "C-on");
    expect(w.legacy.map((m) => m.channel)).toEqual(["C-on"]);
  });

  test("kill switch / disabled built-in (the dispatcher refuses it) → legacy runs, flag or not", async () => {
    // The dispatcher gates the built-in itself (disabledBuiltinsFromConfig);
    // the route sees no slack_brain entry and falls through to legacy.
    const w = windowed({ flagged: [] });
    await post(w.app, "C-on");
    expect(w.legacy.map((m) => m.channel)).toEqual(["C-on"]);
  });

  test("no dispatcher installed yet (boot) → legacy runs", async () => {
    const legacy: SourceMention[] = [];
    const app = makeSlackEventsRoute({
      signingSecret: async () => SECRET,
      ingress: fakeIngress().deps,
      startLegacyThread: async (m) => {
        legacy.push(m);
      },
    });
    await post(app, "C-on");
    expect(legacy.map((m) => m.channel)).toEqual(["C-on"]);
  });
});
