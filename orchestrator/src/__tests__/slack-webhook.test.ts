/**
 * Slack Events API payload classification (ADR 0060 P2.8) — pure, unit-tested.
 *
 * The route verifies the signature with the SDK (`isValidSlackRequest`), then
 * hands the raw body here to classify into the framework's shapes. Field
 * mapping only — verification is the SDK's job.
 */

import { expect, test, describe } from "bun:test";
import { classifySlackEvent } from "../integrations/slack-webhook.ts";

describe("classifySlackEvent", () => {
  test("url_verification → challenge echo", () => {
    expect(classifySlackEvent(JSON.stringify({ type: "url_verification", challenge: "abc123" }))).toEqual({
      kind: "challenge",
      challenge: "abc123",
    });
  });

  test("app_mention in a thread → SourceMention (threadRoot = thread_ts)", () => {
    const body = {
      type: "event_callback",
      team_id: "T1",
      event_id: "Ev1",
      event: { type: "app_mention", channel: "C1", user: "U1", ts: "200.5", thread_ts: "100.0" },
    };
    expect(classifySlackEvent(JSON.stringify(body))).toEqual({
      kind: "mention",
      mention: { team: "T1", channel: "C1", threadRoot: "100.0", user: "U1", ts: "200.5", eventId: "Ev1" },
    });
  });

  test("app_mention NOT in a thread → threadRoot falls back to the message ts", () => {
    const body = {
      type: "event_callback",
      team_id: "T1",
      event_id: "Ev2",
      event: { type: "app_mention", channel: "C1", user: "U1", ts: "300.0" },
    };
    const out = classifySlackEvent(JSON.stringify(body));
    expect(out.kind === "mention" && out.mention.threadRoot).toBe("300.0");
  });

  test("a non-app_mention event → ignore", () => {
    expect(classifySlackEvent(JSON.stringify({ type: "event_callback", event: { type: "message" } }))).toEqual({
      kind: "ignore",
    });
  });

  test("malformed JSON → ignore (never throws)", () => {
    expect(classifySlackEvent("not json")).toEqual({ kind: "ignore" });
  });
});
