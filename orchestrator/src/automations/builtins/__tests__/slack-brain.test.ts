import { describe, expect, test } from "bun:test";

import { matchesIntegrationTrigger, scopeValuesFromInput } from "../../dispatch.ts";
import { registerEngineBlocks } from "../../engine/blocks/index.ts";
import { validateDefinition } from "../../engine/definition.ts";
import { evaluateCode } from "../../code/sandbox.ts";
import {
  SLACK_BRAIN_BUILTIN,
  SLACK_BRAIN_DEFINITION,
  SLACK_FACTS_SOURCE,
} from "../slack-brain.ts";

registerEngineBlocks();

describe("Slack thread brain built-in — definition", () => {
  test("validates as a built-in (system blocks, $ref loop bound, finalize hook)", () => {
    const parsed = validateDefinition(SLACK_BRAIN_DEFINITION, { kind: "builtin" });
    expect(parsed.blocks.map((b) => b.id)).toEqual([
      "facts",
      "admit",
      "session",
      "relay",
      "first_turn",
      "thread",
    ]);
    expect(parsed.settings.concurrency?.policy).toBe("join");
    expect(parsed.settings.endSessionsOnFinish).toBe(false);
    expect(parsed.settings.onFinalize?.[0]?.block.type).toBe("system.slack_thread_recap");
  });

  test("is rejected as a user automation (system blocks are built-in only)", () => {
    expect(() => validateDefinition(SLACK_BRAIN_DEFINITION, { kind: "user" })).toThrow(
      /reserved for built-in/,
    );
  });

  test("trigger scope: only a channel in the inputs' map matches at dispatch", () => {
    const trigger = SLACK_BRAIN_DEFINITION.trigger;
    if (trigger.kind !== "integration") throw new Error("integration trigger expected");
    const bound = { ...trigger, connectionId: "conn-slack" };
    const event = (channel: string, eventKey = "app_mention") => ({
      provider: "slack",
      connectionId: "conn-slack",
      eventKey,
      scopeValue: channel,
    });
    const flagged = { channels: { C1: "prof-a" } };
    expect(matchesIntegrationTrigger(bound, event("C1"), (k) => scopeValuesFromInput(flagged, k))).toBe(true);
    expect(matchesIntegrationTrigger(bound, event("C1", "message"), (k) => scopeValuesFromInput(flagged, k))).toBe(true);
    expect(matchesIntegrationTrigger(bound, event("C2"), (k) => scopeValuesFromInput(flagged, k))).toBe(false);
    // The seeded default (empty map): nothing matches anywhere.
    expect(matchesIntegrationTrigger(bound, event("C1"), (k) => scopeValuesFromInput({ channels: {} }, k))).toBe(false);
    // reaction_added is ledgered by the spine but not a brain event.
    expect(matchesIntegrationTrigger(bound, event("C1", "reaction_added"), (k) => scopeValuesFromInput(flagged, k))).toBe(false);
  });

  test("defaultInputs seeds an empty channel map (nothing flagged, nothing fires)", async () => {
    const inputs = await SLACK_BRAIN_BUILTIN.defaultInputs();
    expect(inputs).toEqual({
      channels: {},
      default_profile: "",
      idle_timeout: 3600,
      max_turns: 50,
    });
  });
});

describe("Slack thread brain — admission code", () => {
  const run = (event: Record<string, unknown>, eventKey: string, inputs: Record<string, unknown>) =>
    evaluateCode(
      SLACK_FACTS_SOURCE,
      { event: { raw: event }, inputs, trigger: { event: eventKey } },
      "value",
    );

  const mention = {
    team_id: "T1",
    event_id: "Ev1",
    event: { type: "app_mention", channel: "C1", user: "U1", ts: "1.1", text: "<@UBOT> hello there" },
  };

  test("an app_mention in a flagged channel admits with the channel's profile", async () => {
    const out = await run(mention, "app_mention", { channels: { C1: "prof-a" } });
    expect(out.ok).toBe(true);
    if (out.ok) {
      expect(out.value).toMatchObject({
        admit: true,
        profile_id: "prof-a",
        team: "T1",
        channel: "C1",
        thread_ts: "1.1",
        mention_ts: "1.1",
        text: "hello there",
        user_id: "U1",
        event_id: "Ev1",
      });
    }
  });

  test("falls back to default_profile; no profile at all → reject", async () => {
    const fb = await run(mention, "app_mention", { channels: {}, default_profile: "prof-d" });
    expect(fb.ok && (fb.value as { profile_id: string }).profile_id).toBe("prof-d");
    const none = await run(mention, "app_mention", { channels: {} });
    expect(none.ok && none.value).toBeNull();
  });

  test("a message event admits only as a thread reply; top-level and bot messages are rejected", async () => {
    const reply = {
      ...mention,
      event: { type: "message", channel: "C1", user: "U2", ts: "1.5", thread_ts: "1.1", text: "more" },
    };
    const ok = await run(reply, "message", { channels: { C1: "p" } });
    expect(ok.ok && (ok.value as { thread_ts: string }).thread_ts).toBe("1.1");

    const topLevel = { ...mention, event: { type: "message", channel: "C1", user: "U2", ts: "2.0", text: "x" } };
    expect((await run(topLevel, "message", { channels: { C1: "p" } })).ok && null).toBeNull();

    const bot = { ...reply, event: { ...reply.event, bot_id: "B1" } };
    const botOut = await run(bot, "message", { channels: { C1: "p" } });
    expect(botOut.ok && botOut.value).toBeNull();

    const edited = { ...reply, event: { ...reply.event, subtype: "message_changed" } };
    const editedOut = await run(edited, "message", { channels: { C1: "p" } });
    expect(editedOut.ok && editedOut.value).toBeNull();
  });
});
