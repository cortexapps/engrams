/**
 * Slack interactivity route (ADR 0060 P2.9) — verification + dispatch.
 *
 * The route verifies with the SDK (`isValidSlackRequest`) over the raw form
 * body, parses the `payload` field via the Block Kit contract, and dispatches:
 * an answer → the thread workflow's mailbox; an "Answer…" click → open a modal.
 * The DBOS/Slack side-effects are injected here so all three branches are
 * unit-testable without the engine.
 */

import { expect, test, describe } from "bun:test";
import { createHmac } from "node:crypto";
import { makeSlackInteractivityRoute } from "../routes/slack-interactivity.ts";
import { ACTION_ANSWER, ACTION_OPEN, ACTION_PROFILE } from "../integrations/slack-blocks.ts";

const SECRET = "test-signing-secret";
const PATH = "/api/v1/integrations/slack/interactivity";
const ROUTE = { team: "T1", channel: "C1", threadRoot: "100.0" };

function makeApp() {
  const calls: { answers: unknown[]; modals: unknown[]; profileChoices: unknown[] } = {
    answers: [],
    modals: [],
    profileChoices: [],
  };
  const app = makeSlackInteractivityRoute({
    signingSecret: async () => SECRET,
    deliverAnswer: async (route, answer) => void calls.answers.push({ route, answer }),
    deliverProfileChoice: async (route, choice, nonce) =>
      void calls.profileChoices.push({ route, choice, nonce }),
    openModal: async (triggerId, view) => void calls.modals.push({ triggerId, view }),
  });
  return { app, calls };
}

/** A profile-select block_actions payload, as Slack sends it. */
function profileSelectPayload(clicker: string) {
  return {
    type: "block_actions",
    user: { id: clicker },
    actions: [
      {
        action_id: ACTION_PROFILE,
        block_id: JSON.stringify({ r: ROUTE, u: "U1", n: "Ev0" }),
        selected_option: { value: "profile-2", text: { type: "plain_text", text: "Infra" } },
      },
    ],
  };
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

  test("the mentioning user's profile pick delivers with the ask's nonce", async () => {
    const { app, calls } = makeApp();
    const res = await postSigned(app, profileSelectPayload("U1"));
    expect(res.status).toBe(200);
    expect(calls.profileChoices).toEqual([
      {
        route: ROUTE,
        choice: { profileId: "profile-2", profileName: "Infra" },
        nonce: "Ev0",
      },
    ]);
  });

  test("a profile pick from a non-requesting user is acked 200 and dropped", async () => {
    const { app, calls } = makeApp();
    const res = await postSigned(app, profileSelectPayload("U9"));
    expect(res.status).toBe(200);
    expect(calls.profileChoices).toHaveLength(0);
  });

  test("a profile pick with malformed block metadata is ignored", async () => {
    const { app, calls } = makeApp();
    const res = await postSigned(app, {
      type: "block_actions",
      user: { id: "U1" },
      actions: [
        { action_id: ACTION_PROFILE, block_id: "not-json", selected_option: { value: "p" } },
      ],
    });
    expect(res.status).toBe(200);
    expect(calls.profileChoices).toHaveLength(0);
  });

  test("an unrelated payload is acked 200 with no side effect", async () => {
    const { app, calls } = makeApp();
    const res = await postSigned(app, { type: "block_actions", actions: [{ action_id: "noop" }] });
    expect(res.status).toBe(200);
    expect(calls.answers).toHaveLength(0);
    expect(calls.modals).toHaveLength(0);
  });
});
