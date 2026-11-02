import { expect, test } from "bun:test";
import {
  assertStableAuthority,
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
