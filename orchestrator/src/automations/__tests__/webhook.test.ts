import { createHash } from "node:crypto";
import { describe, expect, test } from "bun:test";

import {
  extractWebhookEvent,
  matchesWebhookFilter,
  parseWebhookPayload,
  redactWebhookPayload,
} from "../webhook.ts";

const encoded = (value: Record<string, unknown>) =>
  new TextEncoder().encode(JSON.stringify(value));

describe("webhook event extraction", () => {
  // Generic only (ADR 0119 D5): provider-shaped extraction moved to the
  // integration ingress routes.
  test("validates the event header and uses the declared delivery id", () => {
    const rawBody = encoded({ state: "open" });
    expect(extractWebhookEvent({
      headers: new Headers({
        "x-engrams-event": "incident.opened",
        "x-engrams-delivery": "delivery_42",
      }),
      rawBody,
    })).toMatchObject({ eventKey: "incident.opened", deliveryId: "delivery_42" });
  });

  test("falls back to the stable raw-body sha256", () => {
    const rawBody = encoded({ state: "open" });
    const event = extractWebhookEvent({
      headers: new Headers({ "x-engrams-event": "incident.opened" }),
      rawBody,
    });
    expect(event.deliveryId).toBe(createHash("sha256").update(rawBody).digest("hex"));
  });

  test("GitHub-shaped headers are not special-cased any more", () => {
    const rawBody = encoded({ action: "opened" });
    expect(() => extractWebhookEvent({
      headers: new Headers({ "x-github-event": "issues", "x-github-delivery": "d1" }),
      rawBody,
    })).toThrow(/X-Engrams-Event/);
  });

  test("rejects malformed generic event keys", () => {
    const rawBody = encoded({});
    expect(() => extractWebhookEvent({
      headers: new Headers({ "x-engrams-event": "../../bad" }),
      rawBody,
    })).toThrow(/event key/);
  });

  test("parsing requires a UTF-8 JSON object", () => {
    expect(() => parseWebhookPayload(new TextEncoder().encode("[]"))).toThrow(/JSON object/);
  });
});

describe("webhook payload redaction", () => {
  test("strips common secret-bearing keys recursively", () => {
    const redacted = redactWebhookPayload({
      token: "root-token",
      safe: "kept",
      nested: {
        access_token: "nested-token",
        clientSecret: "nested-secret",
        Password: "nested-password",
        headers: { authorization: "Bearer no" },
        items: [{ apiToken: "array-token", value: 42 }],
      },
    });
    expect(redacted).toEqual({
      safe: "kept",
      nested: { headers: {}, items: [{ value: 42 }] },
    });
  });

  test("replaces an unscanned subtree at the depth bound", () => {
    expect(redactWebhookPayload({ a: { b: { password: "hidden" } } }, 2)).toEqual({
      a: { b: "[REDACTED: depth limit]" },
    });
  });

  test("output is a plain prototype-full object with pollution keys dropped", () => {
    // Drizzle's entity check dereferences Object.getPrototypeOf(value) on
    // every inserted field; a null-prototype payload map made the webhook
    // sample insert throw and the ingress 500 (caught by the e2e stack lane).
    const hostile = JSON.parse('{"__proto__":{"polluted":1},"constructor":{"x":1},"ok":{"deep":true}}');
    const redacted = redactWebhookPayload(hostile);
    expect(Object.getPrototypeOf(redacted)).toBe(Object.prototype);
    expect(Object.getPrototypeOf(redacted["ok"])).toBe(Object.prototype);
    expect(Object.keys(redacted)).toEqual(["ok"]);
    expect(({} as Record<string, unknown>)["polluted"]).toBeUndefined();
  });
});

describe("webhook filter semantics", () => {
  const payload = {
    issue: { state: "open", labels: ["bug", "urgent"], author: { id: 7 } },
  };

  test("AND-matches dotted own-property paths by structural JSON equality", () => {
    expect(matchesWebhookFilter(payload, {
      "issue.state": "open",
      "issue.labels": ["bug", "urgent"],
      "issue.author": { id: 7 },
    })).toBe(true);
  });

  test("fails when a path is missing, unsafe, or unequal", () => {
    expect(matchesWebhookFilter(payload, { "issue.missing": null })).toBe(false);
    expect(matchesWebhookFilter(payload, { "issue.__proto__.polluted": true })).toBe(false);
    expect(matchesWebhookFilter(payload, { "issue.state": "closed" })).toBe(false);
  });
});
