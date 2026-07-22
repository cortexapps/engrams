import { createHash } from "node:crypto";
import { describe, expect, test } from "bun:test";

import type { WebhookRegistrationRow } from "../../db/automations.ts";
import {
  extractWebhookEvent,
  matchesWebhookFilter,
  parseWebhookPayload,
  redactWebhookPayload,
} from "../webhook.ts";

const NOW = new Date("2026-07-22T12:00:00Z");
const registration = (
  scheme: WebhookRegistrationRow["verification"]["scheme"],
  providerHint: string | null,
): WebhookRegistrationRow => ({
  id: "example",
  name: "Example",
  verification: { scheme, secretRef: "webhook.example.secret" },
  providerHint,
  createdByUserId: "admin",
  createdAt: NOW,
  updatedAt: NOW,
});

const encoded = (value: Record<string, unknown>) =>
  new TextEncoder().encode(JSON.stringify(value));

describe("webhook event extraction", () => {
  test("GitHub combines event and payload action and reads the delivery id", () => {
    const rawBody = encoded({ action: "opened", issue: { number: 7 } });
    expect(extractWebhookEvent({
      registration: registration("github_hmac_sha256", "github"),
      headers: new Headers({
        "x-github-event": "issues",
        "x-github-delivery": "delivery-1",
      }),
      rawBody,
    })).toMatchObject({ eventKey: "issues.opened", deliveryId: "delivery-1" });
  });

  test("Slack extracts event_callback inner type and event_id", () => {
    const rawBody = encoded({
      type: "event_callback",
      event_id: "Ev123",
      event: { type: "app_mention", text: "hello" },
    });
    expect(extractWebhookEvent({
      registration: registration("slack_v0", "slack"),
      headers: new Headers(),
      rawBody,
    })).toMatchObject({ eventKey: "app_mention", deliveryId: "Ev123" });
  });

  test("generic validates the event header and uses the declared delivery id", () => {
    const rawBody = encoded({ state: "open" });
    expect(extractWebhookEvent({
      registration: registration("generic_hmac_sha256", null),
      headers: new Headers({
        "x-engrams-event": "incident.opened",
        "x-engrams-delivery": "delivery_42",
      }),
      rawBody,
    })).toMatchObject({ eventKey: "incident.opened", deliveryId: "delivery_42" });
  });

  test("generic falls back to the stable raw-body sha256", () => {
    const rawBody = encoded({ state: "open" });
    const event = extractWebhookEvent({
      registration: registration("generic_hmac_sha256", null),
      headers: new Headers({ "x-engrams-event": "incident.opened" }),
      rawBody,
    });
    expect(event.deliveryId).toBe(createHash("sha256").update(rawBody).digest("hex"));
  });

  test("rejects malformed generic event keys", () => {
    const rawBody = encoded({});
    expect(() => extractWebhookEvent({
      registration: registration("generic_hmac_sha256", null),
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
