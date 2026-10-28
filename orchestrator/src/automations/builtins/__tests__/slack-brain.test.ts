import { describe, expect, test } from "bun:test";

import { matchesIntegrationTrigger } from "../../dispatch.ts";
import { registerEngineBlocks } from "../../engine/blocks/index.ts";
import {
  concurrencySettingsOf,
  entrypointsOf,
  instanceSettingsOf,
  validateDefinition,
  walkBlocks,
} from "../../engine/definition.ts";
import { evaluateCode } from "../../code/sandbox.ts";
import {
  SLACK_BRAIN_BUILTIN,
  SLACK_BRAIN_DEFINITION,
  SLACK_FACTS_SOURCE,
  SLACK_RESOLVE_SOURCE,
  SLACK_TRIAGE_SOURCE,
} from "../slack-brain.ts";

registerEngineBlocks();

describe("Slack thread brain built-in — definition", () => {
  test("validates and is an agent with one way in: admit, fold, route, and the engine's agent step", () => {
    const parsed = validateDefinition(SLACK_BRAIN_DEFINITION);
    expect(parsed.blocks.map((b) => b.id)).toEqual([
      "facts",
      "admit",
      "identity",
      "unlinked",
      "linked",
      "previous",
      "replies",
      "turn",
      "has_turn",
      "has_previous",
    ]);
    // Nothing waits: no loop, no wait block anywhere in the graph. The
    // conversation lives in the kept session and its relay, not in a run.
    const types = new Set([...walkBlocks(parsed.blocks)].map((b) => b.type));
    expect(types.has("loop")).toBe(false);
    expect(types.has("wait_event")).toBe(false);
    expect(types.has("wait_session")).toBe(false);
    expect(types.has("send_prompt")).toBe(false);
    expect(types.has("create_session")).toBe(false);
    expect(types.has("run_agent")).toBe(false);
    // A bare mention is not a turn: the gate ends the run before routing.
    expect(parsed.blocks.find((b) => b.id === "has_turn")!.type).toBe("filter");
    // Only a NEW session is routed.
    const routing = parsed.blocks.find((b) => b.id === "has_previous")!;
    expect(routing.then).toEqual([]);
    expect(routing.else!.map((b) => b.id)).toEqual(["smart_route"]);
    // The agent is the definition's: scoped to the thread, it continues the
    // thread's kept session and relays into the thread. The way in ends on
    // its step.
    expect(parsed.agent).toMatchObject({ scope: { kind: "key" }, keepOnFinish: true, relay: { provider: "slack" } });
    const way = entrypointsOf(parsed)[0]!;
    expect(way.blocks.at(-1)).toMatchObject({ id: "agent", type: "run_agent", config: { reuse: "workstream" } });
    // Only a failed run posts into the thread (a plain line that says why);
    // the session's relay is never touched by a run's end.
    expect(parsed.settings.onFinalize?.map((h) => [h.block.type, h.when])).toEqual([["integration_action", ["failed"]]]);
    expect(parsed.settings.onFinalize?.[0]?.block.config).toMatchObject({ actionId: "post_message" });
    // One run at a time per thread: derived from the kept scope.
    expect(parsed.settings.concurrency).toBeUndefined();
    expect(concurrencySettingsOf(parsed)).toEqual({ keyTemplate: "agent", policy: "queue" });
    expect(parsed.settings.runDeadlineSeconds).toBeUndefined();
    expect(parsed.settings.endSessionsOnFinish).toBe(false);
  });

  test("is a workstream per thread (ADR 0120): every mention in the thread is a run of the same workstream", () => {
    const parsed = validateDefinition(SLACK_BRAIN_DEFINITION);
    // Only a mention is an event the brain acts on (the legacy model): the
    // one trigger, no reply entrypoint. A mention in the thread renders the
    // same workstream key as the opening one, so its run finds the thread's
    // kept session.
    expect(parsed.trigger).toMatchObject({ eventKeys: ["app_mention"] });
    expect(parsed.entrypoints ?? []).toEqual([]);
    // The workstream derives from the agent's scope; nothing is declared.
    expect(parsed.settings.instance).toBeUndefined();
    const instance = instanceSettingsOf(parsed)!;
    expect(instance.keyTemplate).toContain('coalesce: "thread_ts", "ts"');
    // The Slack page shows the opening mention's text as the thread's title.
    expect(instance.labelTemplate).toBe("${{ event.raw.event.text | strip_mentions | truncate: 80 }}");
  });

  test("no channel scope: a mention anywhere the app is a member matches; a plain message matches nothing", () => {
    const trigger = SLACK_BRAIN_DEFINITION.trigger;
    if (trigger.kind !== "integration") throw new Error("integration trigger expected");
    const bound = { ...trigger, connectionId: "conn-slack" };
    const event = (channel: string, eventKey = "app_mention") => ({
      provider: "slack",
      connectionId: "conn-slack",
      eventKey,
      scopeValue: channel,
    });
    const none = () => undefined;
    expect(bound.scope).toBeUndefined();
    expect(matchesIntegrationTrigger(bound, event("C1"), none)).toBe(true);
    expect(matchesIntegrationTrigger(bound, event("C-never-seen"), none)).toBe(true);
    // A plain message is never a brain event: it reaches the session only
    // as thread context on the next mention.
    expect(matchesIntegrationTrigger(bound, event("C1", "message"), none)).toBe(false);
    // reaction_added is ledgered by the spine but not a brain event.
    expect(matchesIntegrationTrigger(bound, event("C1", "reaction_added"), none)).toBe(false);
  });

  test("defaultInputs seeds no default profile (the brain answers nowhere until one is set)", async () => {
    const inputs = await SLACK_BRAIN_BUILTIN.defaultInputs();
    expect(inputs).toEqual({
      channels: {},
      default_profile: "",
      routing: "default",
      smart_profiles: [],
      smart_min_confidence: 0.8,
      ask_timeout: 600,
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

  test("the default profile answers everywhere; a channel override wins; no profile at all → reject", async () => {
    const fb = await run(mention, "app_mention", { channels: {}, default_profile: "prof-d" });
    expect(fb.ok && (fb.value as { profile_id: string }).profile_id).toBe("prof-d");
    const over = await run(mention, "app_mention", { channels: { C1: "prof-a" }, default_profile: "prof-d" });
    expect(over.ok && (over.value as { profile_id: string }).profile_id).toBe("prof-a");
    const none = await run(mention, "app_mention", { channels: {} });
    expect(none.ok && none.value).toBeNull();
  });

  test("a message event never opens a thread (replies arrive through the reply entrypoint); bot mentions are rejected", async () => {
    const reply = {
      ...mention,
      event: { type: "message", channel: "C1", user: "U2", ts: "1.5", thread_ts: "1.1", text: "more" },
    };
    const out = await run(reply, "message", { channels: { C1: "p" } });
    expect(out.ok && out.value).toBeNull();

    const bot = { ...mention, event: { ...mention.event, bot_id: "B1" } };
    const botOut = await run(bot, "app_mention", { channels: { C1: "p" } });
    expect(botOut.ok && botOut.value).toBeNull();

    const edited = { ...reply, event: { ...reply.event, subtype: "message_changed" } };
    const editedOut = await run(edited, "message", { channels: { C1: "p" } });
    expect(editedOut.ok && editedOut.value).toBeNull();
  });
});

describe("Slack thread brain — smart routing code", () => {
  const mention = {
    team_id: "T1",
    event_id: "Ev1",
    event: { type: "app_mention", channel: "C1", user: "U1", ts: "1.1", text: "<@UBOT> fix the deploy" },
  };
  const facts = async (inputs: Record<string, unknown>) => {
    const out = await evaluateCode(
      SLACK_FACTS_SOURCE,
      { event: { raw: mention }, inputs, trigger: { event: "app_mention" } },
      "value",
    );
    return out.ok ? (out.value as Record<string, unknown> | null) : undefined;
  };

  test("facts: smart admits without a default; an override is never smart", async () => {
    expect(await facts({ channels: {}, routing: "smart" })).toMatchObject({
      admit: true,
      profile_id: "",
      smart: true,
      override: false,
    });
    expect(await facts({ channels: { C1: "p-o" }, routing: "smart", default_profile: "p-d" })).toMatchObject({
      profile_id: "p-o",
      smart: false,
      override: true,
    });
    expect(await facts({ channels: {}, routing: "default", default_profile: "p-d" })).toMatchObject({
      profile_id: "p-d",
      smart: false,
    });
  });

  const options = [
    { value: "p-web", label: "Web" },
    { value: "p-infra", label: "Infra" },
    { value: "p-d", label: "General" },
  ];
  const decided = (confidence: number, wants = 0.02) => ({
    decided: true,
    answers: {
      profile: { value: "p-infra", confidence, ranked: ["p-infra", "p-web", "p-d"] },
      wants_choice: { yes: wants },
    },
  });
  const triage = async (route: unknown, inputs: Record<string, unknown> = { default_profile: "p-d" }) => {
    const out = await evaluateCode(
      SLACK_TRIAGE_SOURCE,
      { inputs, steps: { route, candidates: { options } } },
      "value",
    );
    return out.ok ? (out.value as { ask: boolean; options: Array<{ value: string }> }) : undefined;
  };

  test("triage: confident → no card; unsure or asked → a card with the ranked picks and the default", async () => {
    expect(await triage(decided(0.9))).toEqual({ ask: false, options: [] });
    const unsure = await triage(decided(0.4));
    expect(unsure!.ask).toBe(true);
    expect(unsure!.options.map((o) => o.value)).toEqual(["p-infra", "p-web", "p-d"]);
    expect((await triage(decided(0.95, 0.8)))!.ask).toBe(true);
    // The org's threshold is honored.
    expect((await triage(decided(0.7), { default_profile: "p-d", smart_min_confidence: 0.8 }))!.ask).toBe(true);
  });

  test("triage: no OpenRouter → never a card; a failed call asks only when there is no default", async () => {
    expect(await triage({ decided: false, reason: "router_not_connected" }, {})).toEqual({ ask: false, options: [] });
    expect((await triage({ decided: false, reason: "decision_failed: 529" }))!.ask).toBe(false);
    expect((await triage({ decided: false, reason: "decision_failed: 529" }, {}))!.ask).toBe(true);
  });

  const resolve = async (steps: Record<string, unknown>) => {
    const out = await evaluateCode(SLACK_RESOLVE_SOURCE, { steps }, "value");
    return out.ok ? (out.value as { profile_id: string; label: string; card: string }) : undefined;
  };
  const smartFacts = { value: { smart: true, profile_id: "p-d" } };

  test("resolve: the click wins, then the model, then the default", async () => {
    const picked = await resolve({
      facts: smartFacts,
      candidates: { options },
      route: decided(0.4),
      ask: { outcome: "answered", value: "p-web" },
    });
    expect(picked).toMatchObject({ profile_id: "p-web", card: "Profile: *Web* (picked in the thread)" });
    const routed = await resolve({ facts: smartFacts, candidates: { options }, route: decided(0.82) });
    expect(routed).toMatchObject({ profile_id: "p-infra", label: " on *Infra* (routed by Jev, 82% confident)" });
    const timedOut = await resolve({
      facts: smartFacts,
      candidates: { options },
      route: decided(0.4),
      ask: { outcome: "deadline" },
    });
    expect(timedOut!.card).toBe("Profile: *Infra* (no answer, so routed by Jev, 40% confident)");
    const fallback = await resolve({
      facts: smartFacts,
      candidates: { options },
      route: { decided: false, reason: "router_not_connected" },
    });
    // No OpenRouter key: exactly default routing, with no routing note.
    expect(fallback).toEqual({ profile_id: "p-d", label: "", card: "" });
    const failed = await resolve({
      facts: smartFacts,
      candidates: { options },
      route: { decided: false, reason: "decision_failed: 529" },
    });
    expect(failed).toMatchObject({ profile_id: "p-d", label: " on *General* (the default profile)" });
    const nobody = await resolve({
      facts: { value: { smart: true, profile_id: "" } },
      candidates: { options },
      route: { decided: false, reason: "decision_failed: 529" },
      ask: { outcome: "deadline" },
    });
    expect(nobody!.profile_id).toBe("");
    // One candidate, no default, the model down: nothing to choose between.
    const sole = await resolve({
      facts: { value: { smart: true, profile_id: "" } },
      candidates: { options: [options[0]] },
      route: { decided: false, reason: "decision_failed: 529" },
    });
    expect(sole).toMatchObject({ profile_id: "p-web", card: "Profile: *Web* (the only profile smart routing can pick)" });
  });
});
