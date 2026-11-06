import { expect, test } from "bun:test";

import { ApiKeySessionCache } from "../auth/api-key-cache.ts";
import type { KeyedSession } from "../auth/api-key-cache.ts";

interface TestSession extends KeyedSession {
  user: { id: string };
}

function sessionFor(keyId: string, key: string, expiresAt = new Date(10_000_000)): TestSession {
  return { user: { id: `u-${keyId}` }, session: { id: keyId, token: key, expiresAt } };
}

function cacheAt(start: number, ttlMs = 5_000) {
  const clock = { now: start };
  return { clock, cache: new ApiKeySessionCache<TestSession>(ttlMs, () => clock.now) };
}

test("a hit returns the session with the presented key as its token", () => {
  const { cache } = cacheAt(1_000);
  cache.set("engk_a", sessionFor("k1", "engk_a"));
  const hit = cache.get("engk_a");
  expect(hit?.user.id).toBe("u-k1");
  expect(hit?.session.token).toBe("engk_a");
  expect(cache.get("engk_b")).toBeNull();
});

test("an entry expires after the TTL", () => {
  const { clock, cache } = cacheAt(1_000, 5_000);
  cache.set("engk_a", sessionFor("k1", "engk_a"));
  clock.now = 5_999;
  expect(cache.get("engk_a")).not.toBeNull();
  clock.now = 6_000;
  expect(cache.get("engk_a")).toBeNull();
});

test("an entry never outlives the key's own expiry", () => {
  const { clock, cache } = cacheAt(1_000, 5_000);
  cache.set("engk_a", sessionFor("k1", "engk_a", new Date(2_000)));
  clock.now = 2_000;
  expect(cache.get("engk_a")).toBeNull();
  cache.set("engk_b", sessionFor("k2", "engk_b", new Date(1_500)));
  expect(cache.get("engk_b")).toBeNull();
});

test("forgetKey drops the key by id and keeps other keys", () => {
  const { cache } = cacheAt(1_000);
  cache.set("engk_a", sessionFor("k1", "engk_a"));
  cache.set("engk_b", sessionFor("k2", "engk_b"));
  cache.forgetKey("k1");
  expect(cache.get("engk_a")).toBeNull();
  expect(cache.get("engk_b")).not.toBeNull();
});

test("the cache stays bounded and drops the oldest live entry first", () => {
  const { cache } = cacheAt(1_000);
  for (let i = 0; i <= 1_024; i++) cache.set(`engk_${i}`, sessionFor(`k${i}`, `engk_${i}`));
  expect(cache.get("engk_0")).toBeNull();
  expect(cache.get("engk_1")).not.toBeNull();
  expect(cache.get("engk_1024")).not.toBeNull();
});
