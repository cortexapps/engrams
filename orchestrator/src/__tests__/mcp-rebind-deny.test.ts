import { expect, test } from "bun:test";
import { canonicalDigest } from "../mcp/hash.ts";
import { prepareRebindDeny } from "../mcp/rebind-deny.ts";

function policy(revision: number, hosts: string[]) {
  const body = { version: 1 as const, revision, hosts };
  return { ...body, digest: canonicalDigest(body) };
}

test("lost acknowledgement retains the host-applied deny set at a newer revision", () => {
  const old = policy(2, ["old.example.test"]);
  const next = policy(3, ["new.example.test", "parent.example.test"]);
  const applied = policy(3, ["lost.example.test", "old.example.test"]);
  const host = {
    sessionId: "s1",
    bindingEpoch: 7,
    bindingHash: `sha256:${"a".repeat(64)}`,
    policy: applied,
  };
  const result = prepareRebindDeny({ sessionId: "s1", bindingEpoch: 7, old, next, host });
  expect(result.revision).toBe(4);
  expect(result.hosts).toEqual([
    "lost.example.test",
    "new.example.test",
    "old.example.test",
    "parent.example.test",
  ]);
  expect(result.digest).toBe(canonicalDigest({ version: 1, revision: 4, hosts: result.hosts }));
});

test("untrusted host identity and corrupted applied policy fail closed", () => {
  const old = policy(1, ["old.example.test"]);
  const host = {
    sessionId: "s1",
    bindingEpoch: 7,
    bindingHash: `sha256:${"a".repeat(64)}`,
    policy: old,
  };
  expect(() =>
    prepareRebindDeny({ sessionId: "s1", bindingEpoch: 8, old, next: old, host }),
  ).toThrow();
  expect(() =>
    prepareRebindDeny({
      sessionId: "s1",
      bindingEpoch: 7,
      old,
      next: old,
      host: { ...host, policy: { ...old, hosts: [] } },
    }),
  ).toThrow();
});

test("host apply is compare-and-set and validates the exact acknowledged binding", async () => {
  const { applyRebindDeny } = await import("../mcp/rebind-deny.ts");
  const old = policy(1, ["old.example.test"]);
  const host = {
    sessionId: "s1",
    bindingEpoch: 7,
    bindingHash: `sha256:${"a".repeat(64)}`,
    policy: old,
  };
  const next = prepareRebindDeny({ sessionId: "s1", bindingEpoch: 7, old, next: old, host });
  const expected = {
    sessionId: "s1",
    bindingEpoch: 7,
    bindingHash: `sha256:${"b".repeat(64)}`,
    denyRevision: next.revision,
    denyDigest: next.digest,
  };
  let installed = host;
  const fixture = {
    async compareAndApply(previous: typeof host, desired: typeof expected, deny: typeof next) {
      if (canonicalDigest(previous) !== canonicalDigest(installed))
        throw Error("host state changed");
      installed = { ...host, bindingHash: desired.bindingHash, policy: deny };
      return desired;
    },
  };
  await applyRebindDeny(fixture, host, expected, next);
  await expect(applyRebindDeny(fixture, host, expected, next)).rejects.toThrow(
    "host state changed",
  );
  const wrongAck = {
    ...fixture,
    async compareAndApply() {
      return { ...expected, bindingHash: host.bindingHash };
    },
  };
  await expect(applyRebindDeny(wrongAck, installed, expected, next)).rejects.toThrow();
});
