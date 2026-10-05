/**
 * Slack interactivity route (ADR 0060 P2.9) — verification + dispatch.
 *
 * The route verifies with the SDK (`isValidSlackRequest`) over the raw form
 * body, parses the `payload` field via the Block Kit contract, and dispatches:
 * an answer → the run whose relay asked; an "Answer…" click → open a modal.
 * The DBOS/Slack side-effects are injected here so both branches are
 * unit-testable without the engine.
 */

import { expect, test, describe } from "bun:test";
import { createHmac } from "node:crypto";
import { answerDelivery, makeSlackInteractivityRoute } from "../routes/slack-interactivity.ts";
import { ACTION_ANSWER, ACTION_OPEN } from "../integrations/slack-blocks.ts";

const SECRET = "test-signing-secret";
const PATH = "/api/v1/integrations/slack/interactivity";
const ROUTE = { team: "T1", channel: "C1", threadRoot: "100.0" };

function makeApp() {
  const calls: { answers: unknown[]; modals: unknown[] } = { answers: [], modals: [] };
  const app = makeSlackInteractivityRoute({
    signingSecret: async () => SECRET,
    deliverAnswer: async (route, answer) => void calls.answers.push({ route, answer }),
    openModal: async (triggerId, view) => void calls.modals.push({ triggerId, view }),
  });
  return { app, calls };
}

function postSigned(app: ReturnType<typeof makeApp>["app"], payload: unknown) {
  const body = "payload=" + encodeURIComponent(JSON.stringify(payload));
  const ts = String(Math.floor(Date.now() / 1000));
  const sig = "v0=" + createHmac("sha256", SECRET).update(`v0:${ts}:${body}`).digest("hex");
  return app.request(PATH, {
    method: "POST",
    body,
    headers: {
      "content-type": "application/x-www-form-urlencoded",
      "x-slack-signature": sig,
      "x-slack-request-timestamp": ts,
    },
  });
}

describe("POST /api/v1/integrations/slack/interactivity", () => {
  test("rejects an invalid signature with 401", async () => {
    const { app, calls } = makeApp();
    const res = await app.request(PATH, {
      method: "POST",
      body: "payload=%7B%7D",
      headers: {
        "content-type": "application/x-www-form-urlencoded",
        "x-slack-signature": "v0=deadbeef",
        "x-slack-request-timestamp": String(Math.floor(Date.now() / 1000)),
      },
    });
    expect(res.status).toBe(401);
    expect(calls.answers).toHaveLength(0);
    expect(calls.modals).toHaveLength(0);
  });

  test("an inline option click delivers an answer to the thread", async () => {
    const { app, calls } = makeApp();
    const res = await postSigned(app, {
      type: "block_actions",
      trigger_id: "trig",
      actions: [
        { action_id: ACTION_ANSWER, value: JSON.stringify({ t: "tc1", q: "Pick", a: "A", r: ROUTE }) },
      ],
    });
    expect(res.status).toBe(200);
    expect(calls.answers).toEqual([
      { route: ROUTE, answer: { toolCallId: "tc1", answers: { Pick: ["A"] } } },
    ]);
    expect(calls.modals).toHaveLength(0);
  });

  test("an 'Answer…' click opens a modal", async () => {
    const { app, calls } = makeApp();
    const res = await postSigned(app, {
      type: "block_actions",
      trigger_id: "trig42",
      actions: [
        {
          action_id: ACTION_OPEN,
          value: JSON.stringify({
            t: "tc2",
            qs: [{ q: "Many?", h: "Many", m: true, o: ["X", "Y"] }],
            r: ROUTE,
          }),
        },
      ],
    });
    expect(res.status).toBe(200);
    expect(calls.modals).toHaveLength(1);
    expect((calls.modals[0] as { triggerId: string }).triggerId).toBe("trig42");
    expect(calls.answers).toHaveLength(0);
  });

  test("an unrelated payload is acked 200 with no side effect", async () => {
    const { app, calls } = makeApp();
    const res = await postSigned(app, { type: "block_actions", actions: [{ action_id: "noop" }] });
    expect(res.status).toBe(200);
    expect(calls.answers).toHaveLength(0);
    expect(calls.modals).toHaveLength(0);
  });
});


// ---------------------------------------------------------------------------
// The automation run's mailbox (ADR 0119 phase 4.5; the only path since 4.8)
// ---------------------------------------------------------------------------

describe("automation relay answers", () => {
  const RUN_ROUTE = { ...ROUTE, runId: "autorun:auto-1:slack:E1" };

  test("a route carrying runId delivers a slack_answer signal to that run; a card without one is unrouted", () => {
    const answer = { toolCallId: "tc1", answers: { Pick: ["A"] } };
    expect(answerDelivery(RUN_ROUTE, answer)).toEqual({
      kind: "automation",
      runId: "autorun:auto-1:slack:E1",
      message: {
        kind: "signal",
        name: "slack_answer",
        payload: { toolCallId: "tc1", answers: { Pick: ["A"] } },
      },
      idempotencyKey: "slack-answer:tc1",
    });
    expect(answerDelivery(ROUTE, answer)).toEqual({ kind: "unrouted" });
  });

  test("the runId rides the Block Kit value end to end through the route", async () => {
    const { app, calls } = makeApp();
    const res = await postSigned(app, {
      type: "block_actions",
      trigger_id: "trig",
      actions: [
        {
          action_id: ACTION_ANSWER,
          value: JSON.stringify({ t: "tc9", q: "Pick", a: "B", r: RUN_ROUTE }),
        },
      ],
    });
    expect(res.status).toBe(200);
    expect(calls.answers).toEqual([
      { route: RUN_ROUTE, answer: { toolCallId: "tc9", answers: { Pick: ["B"] } } },
    ]);
  });
});
