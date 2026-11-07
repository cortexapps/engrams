import { expect, test } from "bun:test";
import {
  assertStableAuthority,
  assertOAuthRefusal,
  assertOAuthRefreshFailure,
  assertInvalidatedAuthority,
  oauthCallPlan,
} from "./case-oauth.ts";

test("ordinary token rotation cannot silently replace bound authority", () => {
  const before = {
    bindingHash: "sha256:binding",
    current: true,
    authGeneration: 3,
    credentialIdentity: "sha256:identity",
  };
  expect(() => assertStableAuthority(before, before)).not.toThrow();
  for (const after of [
    { ...before, bindingHash: "sha256:new" },
    { ...before, current: false },
    { ...before, authGeneration: 4 },
    { ...before, credentialIdentity: "sha256:other" },
  ])
    expect(() => assertStableAuthority(before, after)).toThrow();
});
test("authority changes must stale the original frozen binding", () => {
  expect(() =>
    assertInvalidatedAuthority("old", {
      bindingHash: "old",
      current: false,
      staleReasons: ["credential_changed"],
    }),
  ).not.toThrow();
  for (const after of [
    { bindingHash: "new", current: true, staleReasons: [] },
    { bindingHash: "old", current: false, staleReasons: [] },
  ])
    expect(() => assertInvalidatedAuthority("old", after)).toThrow();
});
test("OAuth retries use bounded distinct immutable native call IDs", () => {
  const plan = oauthCallPlan("mcp_fixture");
  expect(plan.steps.length).toBeLessThanOrEqual(16);
  expect(new Set(plan.steps.map((s) => s.callId)).size).toBe(plan.steps.length);
  expect(
    plan.steps.every(
      (s) => s.tool === "mcp_fixture" && s.resultFormat === "mcp",
    ),
  ).toBe(true);
});

test("changed OAuth authority requires a received available runtime refusal", () => {
  const refusal = {
    resultReceived: true,
    delivery: "available",
    outcome: null,
    isError: true,
  };
  expect(() => assertOAuthRefusal(refusal)).not.toThrow();
  for (const invalid of [
    {},
    { ...refusal, resultReceived: false },
    { ...refusal, delivery: "unavailable" },
    { ...refusal, delivery: undefined },
    { ...refusal, isError: undefined },
    { ...refusal, isError: false },
    { ...refusal, outcome: undefined },
    { ...refusal, outcome: "succeeded" },
    { ...refusal, outcome: "failed" },
    { ...refusal, outcome: "outcome_unknown" },
    { ...refusal, outcome: "canceled" },
    {
      resultReceived: true,
      delivery: "unavailable",
      outcome: "failed",
      isError: null,
    },
  ])
    expect(() => assertOAuthRefusal(invalid)).toThrow();
});

test("refresh-trigger failure requires the exact persisted unavailable read failure", () => {
  const proof = {
    resultReceived: true,
    delivery: "unavailable",
    outcome: "failed",
    isError: null,
    resultUnavailable: false,
  };
  const result = {
    state: "failed",
    resultUnavailable: false,
    resultJson: JSON.stringify({
      outcome: "failed",
      delivery: "unavailable",
      reason: "upstream_failure",
    }),
  };
  expect(() => assertOAuthRefreshFailure(proof, result)).not.toThrow();
  for (const invalid of [
    { ...proof, resultReceived: false },
    { ...proof, outcome: null },
    { ...proof, outcome: "canceled" },
    { ...proof, outcome: "outcome_unknown" },
    { ...proof, delivery: "available" },
    { ...proof, isError: true },
    { ...proof, resultUnavailable: true },
  ])
    expect(() => assertOAuthRefreshFailure(invalid, result)).toThrow();
  for (const invalid of [
    { ...result, state: "canceled" },
    { ...result, resultUnavailable: true },
    { ...result, resultJson: undefined },
    {
      ...result,
      resultJson: JSON.stringify({
        outcome: "failed",
        delivery: "unavailable",
        reason: "execution_interrupted",
      }),
    },
    {
      ...result,
      resultJson: JSON.stringify({
        outcome: "failed",
        delivery: "unavailable",
        reason: "upstream_failure",
        output: { isError: true },
      }),
    },
  ])
    expect(() => assertOAuthRefreshFailure(proof, invalid)).toThrow();
});

test("each authority change has a distinct immutable stale refusal call before recovery", () => {
  const plan = oauthCallPlan("mcp_fixture");
  for (const change of ["account", "scope", "reconnect"]) {
    const trigger = plan.steps.find((step) => step.marker === change);
    const refusal = plan.steps.find(
      (step) => step.marker === `${change}-stale`,
    );
    expect(refusal).toBeDefined();
    expect(refusal?.callId).not.toBe(trigger?.callId);
    expect(plan.steps.indexOf(refusal!)).toBeGreaterThan(
      plan.steps.indexOf(trigger!),
    );
    expect(plan.steps.indexOf(refusal!)).toBeLessThan(
      plan.steps.findIndex((step) => step.marker === `${change}-rebound`),
    );
  }
});
