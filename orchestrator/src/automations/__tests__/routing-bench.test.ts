import { describe, expect, test } from "bun:test";

import {
  sampleState,
  scorableSamples,
  scoreSlice,
  type RoutingResult,
  type RoutingSample,
} from "../routing-bench.ts";
import type { ProfileCard } from "../profile-cards.ts";

const card = (id: string): ProfileCard => ({
  id,
  name: id,
  description: "",
  repos: [],
  skills: [],
  integrations: [],
  allowHosts: [],
  envVarNames: [],
});

function sample(id: string, label: string, extra: Partial<RoutingSample> = {}): RoutingSample {
  return { slice: "web", id, created_at: `2026-01-0${id}T00:00:00Z`, label, title: `req ${id}`, ...extra };
}

function result(s: RoutingSample, predicted: string, confidence: number, ranked?: string[]): RoutingResult {
  return { sample: s, predicted, confidence, ranked: ranked ?? [predicted], wantsChoice: 0, latencyMs: 100, cost: 0.00001 };
}

describe("sampleState", () => {
  test("prefers the full prompt, strips mentions, keeps the gist", () => {
    expect(sampleState(sample("1", "a", { prompt: "<@U1> fix the build", summary: "Fix CI" }))).toEqual({
      message: "fix the build",
      gist: "Fix CI",
    });
    expect(sampleState(sample("1", "a", { title: null, summary: null }))).toBeNull();
  });
});

describe("scorableSamples", () => {
  test("drops labels that are no longer profiles and empty requests", () => {
    const kept = scorableSamples(
      [sample("1", "a"), sample("2", "gone"), sample("3", "a", { title: "" })],
      [card("a")],
    );
    expect(kept.map((s) => s.id)).toEqual(["1"]);
  });
});

describe("scoreSlice", () => {
  test("accuracy, coverage by threshold, errors and the priors", () => {
    const s1 = sample("1", "a", { user: "u1" });
    const s2 = sample("2", "a", { user: "u1" });
    const s3 = sample("3", "b", { user: "u1" });
    const s4 = sample("4", "b", { user: "u2" });
    const results: RoutingResult[] = [
      result(s1, "a", 0.9),
      result(s2, "b", 0.4, ["b", "a"]),
      result(s3, "b", 0.8),
      { sample: s4, latencyMs: 5, error: "boom" },
    ];
    const report = scoreSlice("web", results, [s1, s2, s3, s4]);
    expect(report.n).toBe(4);
    expect(report.errors).toBe(1);
    expect(report.top1).toBe(0.5);
    expect(report.top2).toBe(0.75);
    const at7 = report.coverage.find((c) => c.threshold === 0.7)!;
    expect(at7).toEqual({ threshold: 0.7, coverage: 0.5, accuracy: 1 });
    expect(report.confusion).toEqual({ a: { a: 1, b: 1 }, b: { b: 1 } });
    // Chronological user prior: s1 has no history (global majority so far =
    // none → wrong), s2 → a (right), s3 → a (wrong), s4 u2 has none → global
    // majority a (wrong).
    expect(report.baselines.userPrior).toBe(0.25);
    expect(report.baselines.channelPrior).toBeNull();
    expect(report.baselines.majority).toBe(0.5);
  });
});
