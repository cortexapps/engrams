import { describe, expect, test } from "bun:test";

import { matchesIntegrationTrigger } from "../../dispatch.ts";
import { registerEngineBlocks } from "../../engine/blocks/index.ts";
import { validateDefinition } from "../../engine/definition.ts";
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
  test("validates ($ref loop bound, finalize hook) and every block is a palette block", () => {
    const parsed = validateDefinition(SLACK_BRAIN_DEFINITION);
    expect(parsed.blocks.map((b) => b.id)).toEqual([
      "facts",
      "admit",
      "identity",
      "unlinked",
      "linked",
      "previous",
      "replies",
      "opening",
      "has_previous",
      "pick",
      "relay",
      "opening_turn",
      "thread",
    ]);
    // The idle exit (completed) neither posts nor closes the workstream; only
    // an explicit end closes it, so the next mention resumes the session.
    const hooks = parsed.settings.onFinalize!;
    expect(hooks.find((h) => h.block.type === "relay_close")!.when).not.toContain("completed");
    // Nothing time-based fails a thread: the run ceiling is a pause too.
    expect(hooks.find((h) => h.block.type === "relay_close")!.when).not.toContain("deadline");
    expect(hooks.find((h) => h.block.type === "instance_close")!.when).not.toContain("deadline");
    expect(hooks.find((h) => h.block.type === "instance_close")!.when).toEqual(["halted", "superseded"]);
    expect(parsed.settings.concurrency?.policy).toBe("join");
    expect(parsed.settings.endSessionsOnFinish).toBe(false);
    expect(parsed.settings.onFinalize?.map((h) => h.block.type)).toEqual(["relay_close", "instance_close"]);
  });

  test("is a workstream per thread (ADR 0120): a mention opens it, a later mention in the thread joins it", () => {
    const parsed = validateDefinition(SLACK_BRAIN_DEFINITION);
    // Only a mention is an event the brain acts on (the legacy model): the
    // one trigger, no reply entrypoint. A mention in the thread renders the
    // same workstream key as the opening one, so it binds and joins.
    expect(parsed.trigger).toMatchObject({ eventKeys: ["app_mention"] });
    expect(parsed.entrypoints ?? []).toEqual([]);
    expect(parsed.settings.instance?.keyTemplate).toBe(parsed.settings.concurrency?.keyTemplate);
    expect(parsed.settings.instance?.keyTemplate).toContain('coalesce: "thread_ts", "ts"');
    // The Slack page shows the opening mention's text as the thread's title.
    expect(parsed.settings.instance?.labelTemplate).toBe("${{ event.raw.event.text | strip_mentions | truncate: 80 }}");
    // The loop's wait consumes mentions only; a plain reply is folded into
    // the next mention's prompt by the list_replies + code pair.
    const thread = parsed.blocks.find((b) => b.id === "thread")!;
    const next = thread.body!.find((b) => b.id === "next")!;
    expect(next.config["eventKeys"]).toEqual(["app_mention"]);
    expect(thread.body!.map((b) => b.id)).toEqual(["next", "has_event"]);
    const onEvent = thread.body!.find((b) => b.id === "has_event")!;
    expect(onEvent.then!.map((b) => b.id)).toEqual(["thread_replies", "turn_text", "has_turn"]);
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
  });
});
