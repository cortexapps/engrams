/**
 * Profile picker decision flow (the LLM is faked; the production model call is
 * one injected dep). Under test: the deterministic short-circuits, the
 * ranking normalization (unknown ids dropped, fallback fills the tail), and
 * the never-drop guarantee — any model failure degrades to ask_user in
 * histogram order, never to a guess and never to an error.
 */

import { describe, expect, test } from "bun:test";

import {
  buildPickerPrompt,
  fallbackOrder,
  makePicker,
  normalizeDecision,
  type Histogram,
  type ProfileCard,
  type ProfilePickerDeps,
  type RawPickDecision,
} from "../profile-picker.ts";

const card = (id: string, name: string, over: Partial<ProfileCard> = {}): ProfileCard => ({
  id,
  name,
  description: `${name} profile`,
  skills: [],
  connectorProviders: [],
  allowHosts: [],
  envVarNames: [],
  ...over,
});

const INPUT = { team: "T1", channel: "C1", ownerUserId: "u1", prompt: "fix the deploy" };

const CANDIDATES = [card("a", "Backend"), card("b", "Infra"), card("c", "Web")];

function deps(over: Partial<ProfilePickerDeps> = {}): ProfilePickerDeps {
  return {
    loadCandidates: async () => CANDIDATES,
    channelHistogram: async () => new Map([["b", 12], ["a", 2]]),
    userHistogram: async () => new Map([["a", 5]]),
    generatePick: async () => ({ decision: "route", ranked: ["b", "a", "c"], reason: "r" }),
    ...over,
  };
}

describe("fallbackOrder", () => {
  test("orders by channel count, then user count, then name", () => {
    const channel: Histogram = new Map([["b", 3]]);
    const user: Histogram = new Map([["c", 1]]);
    expect(fallbackOrder(CANDIDATES, channel, user).map((c) => c.id)).toEqual(["b", "c", "a"]);
  });
});

describe("normalizeDecision", () => {
  const fallback = fallbackOrder(CANDIDATES, new Map(), new Map());

  test("a valid route returns the top profile", () => {
    const raw: RawPickDecision = { decision: "route", ranked: ["c", "a"], reason: "" };
    expect(normalizeDecision(raw, CANDIDATES, fallback)).toEqual({
      decision: "route",
      profile: { id: "c", name: "Web", description: "Web profile" },
    });
  });

  test("a route whose top id is unknown degrades to ask_user", () => {
    const raw: RawPickDecision = { decision: "route", ranked: ["ghost", "a"], reason: "" };
    const pick = normalizeDecision(raw, CANDIDATES, fallback);
    expect(pick.decision).toBe("ask_user");
  });

  test("ask_user keeps the model's order, drops unknowns, fills the tail", () => {
    const raw: RawPickDecision = { decision: "ask_user", ranked: ["c", "ghost", "c"], reason: "" };
    const pick = normalizeDecision(raw, CANDIDATES, fallback);
    if (pick.decision !== "ask_user") throw new Error("expected ask_user");
    // c first (the model's pick), then the fallback order minus c.
    expect(pick.options.map((o) => o.id)).toEqual(["c", "a", "b"]);
  });
});

describe("makePicker", () => {
  test("zero active profiles → none", async () => {
    const picker = makePicker(deps({ loadCandidates: async () => [] }));
    expect(await picker.pick(INPUT)).toEqual({ decision: "none" });
  });

  test("one active profile routes with no model call", async () => {
    let called = false;
    const picker = makePicker(
      deps({
        loadCandidates: async () => [card("only", "Solo")],
        generatePick: async () => {
          called = true;
          return { decision: "route", ranked: ["only"], reason: "" };
        },
      }),
    );
    const pick = await picker.pick(INPUT);
    expect(pick).toEqual({
      decision: "route",
      profile: { id: "only", name: "Solo", description: "Solo profile" },
    });
    expect(called).toBe(false);
  });

  test("a confident model decision routes to its top profile", async () => {
    const picker = makePicker(deps());
    const pick = await picker.pick(INPUT);
    expect(pick).toEqual({
      decision: "route",
      profile: { id: "b", name: "Infra", description: "Infra profile" },
    });
  });

  test("a model failure degrades to ask_user in histogram order — never a guess", async () => {
    const picker = makePicker(
      deps({
        generatePick: async () => {
          throw new Error("model timeout");
        },
      }),
    );
    const pick = await picker.pick(INPUT);
    if (pick.decision !== "ask_user") throw new Error("expected ask_user");
    // channel: b=12, a=2; user: a=5 — so b, a, then c by name.
    expect(pick.options.map((o) => o.id)).toEqual(["b", "a", "c"]);
  });

  test("the model prompt carries the message, both histograms, and every card", async () => {
    let prompt = "";
    const picker = makePicker(
      deps({
        generatePick: async (p) => {
          prompt = p;
          return { decision: "route", ranked: ["a"], reason: "" };
        },
      }),
    );
    await picker.pick(INPUT);
    const parsed = JSON.parse(prompt) as {
      message: string;
      channelHistory: { profileId: string; profile: string; recentSessions: number }[];
      userHistory: { profileId: string; profile: string; recentSessions: number }[];
      profiles: { id: string }[];
    };
    expect(parsed.message).toBe("fix the deploy");
    expect(parsed.channelHistory).toEqual([
      { profileId: "b", profile: "Infra", recentSessions: 12 },
      { profileId: "a", profile: "Backend", recentSessions: 2 },
    ]);
    expect(parsed.userHistory).toEqual([
      { profileId: "a", profile: "Backend", recentSessions: 5 },
    ]);
    expect(parsed.profiles.map((p) => p.id)).toEqual(["a", "b", "c"]);
  });
});

describe("buildPickerPrompt", () => {
  test("drops histogram entries for archived/unknown profiles", () => {
    const prompt = buildPickerPrompt(
      INPUT,
      CANDIDATES,
      new Map([["gone", 9], ["a", 1]]),
      new Map(),
    );
    const parsed = JSON.parse(prompt) as { channelHistory: { profileId: string }[] };
    expect(parsed.channelHistory.map((h) => h.profileId)).toEqual(["a"]);
  });
});
