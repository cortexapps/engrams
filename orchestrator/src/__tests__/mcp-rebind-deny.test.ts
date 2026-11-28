import { expect, test } from "bun:test";
import { canonicalDigest } from "../mcp/hash.ts";
import { unionManagedPolicies } from "../mcp/managed-policy.ts";
function policy(revision: number, hosts: string[]) {
  const body = { version: 1 as const, revision, hosts };
  return { ...body, digest: canonicalDigest(body) };
}
test("cold deny union retains old, new and lost acknowledgement hosts at a newer revision", () => {
  const result = unionManagedPolicies([
    policy(2, ["old.example.test"]),
    policy(3, ["new.example.test", "parent.example.test"]),
    policy(3, ["lost.example.test", "old.example.test"]),
  ]);
  expect(result.revision).toBe(4);
  expect(result.hosts).toEqual([
    "lost.example.test",
    "new.example.test",
    "old.example.test",
    "parent.example.test",
  ]);
  expect(result.digest).toBe(canonicalDigest({ version: 1, revision: 4, hosts: result.hosts }));
});
test("cold deny union rejects corrupted retained policy", () => {
  expect(() => unionManagedPolicies([{ ...policy(1, ["old.example.test"]), hosts: [] }])).toThrow();
});
