import { expect, test } from "bun:test";
import { toWorkerBindingHash, assertWorkerPolicy } from "../mcp/worker-encoding.ts";

test("worker digests use strict representation conversion without changing frozen identity", () => {
  const raw = "a".repeat(64);
  const frozen = { zoneId: "public", policyRevision: "1", policyDigest: `sha256:${raw}` };
  const identity = { protocol: 1 as const, bootId: "boot", ...frozen, policyDigest: raw };
  expect(toWorkerBindingHash(`sha256:${raw}`)).toBe(raw);
  expect(() => assertWorkerPolicy(identity, frozen)).not.toThrow();
  expect(frozen.policyDigest).toBe(`sha256:${raw}`);
  expect(identity.policyDigest).toBe(raw);
  for (const value of [
    raw,
    `sha256:sha256:${raw}`,
    `SHA256:${raw}`,
    `sha256:${raw.toUpperCase()}`,
    `sha256:${raw} `,
    `sha256:${raw.slice(1)}`,
  ]) {
    expect(() => toWorkerBindingHash(value)).toThrow();
  }
  for (const policyDigest of [
    `sha256:${raw}`,
    `sha256:sha256:${raw}`,
    raw.toUpperCase(),
    "b".repeat(64),
    `${raw} `,
  ]) {
    expect(() => assertWorkerPolicy({ ...identity, policyDigest }, frozen)).toThrow();
  }
  expect(() => assertWorkerPolicy({ ...identity, zoneId: "other" }, frozen)).toThrow();
  expect(() => assertWorkerPolicy({ ...identity, policyRevision: "2" }, frozen)).toThrow();
  expect(() => assertWorkerPolicy(identity, { ...frozen, policyDigest: raw })).toThrow();
});
