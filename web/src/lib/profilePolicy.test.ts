/**
 * derivePolicy's harness-egress fold (ADR 0063 addendum) — the client-side
 * mirror of the coordinator's merge_harness_egress: deny-default networks
 * gain the selected harness's declared hosts; allow-default networks don't.
 */

import { describe, expect, it } from "vitest";

import { derivePolicy } from "./profilePolicy";

const draft = (dflt: "deny" | "allow", hosts: string[] = []) => ({
  capabilities: [],
  network: { default: dflt, allowHosts: hosts, allowHostPatterns: [] },
  secrets: [],
});

const CLAUDE_EGRESS = {
  allowHosts: ["api.anthropic.com", "statsig.anthropic.com"],
  allowHostPatterns: [],
};

describe("derivePolicy harness egress", () => {
  it("folds harness hosts into reachable on a deny-default network", () => {
    const p = derivePolicy(draft("deny", ["github.com"]), [], CLAUDE_EGRESS);
    expect(p.harnessHosts).toEqual(["api.anthropic.com", "statsig.anthropic.com"]);
    expect(p.reachable).toEqual(["github.com", "api.anthropic.com", "statsig.anthropic.com"]);
  });

  it("dedupes hosts the profile already lists", () => {
    const p = derivePolicy(draft("deny", ["api.anthropic.com"]), [], CLAUDE_EGRESS);
    expect(p.reachable).toEqual(["api.anthropic.com", "statsig.anthropic.com"]);
  });

  it("adds nothing on an allow-default network (everything is reachable)", () => {
    const p = derivePolicy(draft("allow"), [], CLAUDE_EGRESS);
    expect(p.harnessHosts).toEqual([]);
    expect(p.reachable).toEqual([]);
  });

  it("is unchanged when no harness egress is passed", () => {
    const p = derivePolicy(draft("deny", ["github.com"]), []);
    expect(p.harnessHosts).toEqual([]);
    expect(p.reachable).toEqual(["github.com"]);
  });
});
