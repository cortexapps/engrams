import { describe, expect, test } from "vitest";

import type { ConnectorCapabilityView } from "@/components/integrations/useConnectorViews";
import {
  MINIMUM_PERMISSIONS,
  WEBHOOK_EVENTS,
  buildGithubManifest,
  githubWebhookUrl,
  isMinimumPermission,
  permissionLabel,
  permissionsForCapabilities,
} from "./githubSetup";

const cap = (action: string, access: "read" | "write"): ConnectorCapabilityView => ({
  action,
  access,
});

describe("permissionsForCapabilities", () => {
  test("always includes the minimum floor, even with no capabilities", () => {
    const perms = permissionsForCapabilities([]);
    for (const min of MINIMUM_PERMISSIONS) {
      expect(perms).toContainEqual(min);
    }
  });

  test("maps `pulls` → `pull_requests` (mirrors the coordinator)", () => {
    const perms = permissionsForCapabilities([cap("pulls:write", "write")]);
    expect(perms.find((p) => p.key === "pull_requests")?.level).toBe("write");
    expect(perms.some((p) => p.key === "pulls")).toBe(false);
  });

  test("adds a permission per granted power at the highest level needed", () => {
    const perms = permissionsForCapabilities([
      cap("actions:read", "read"),
      cap("actions:write", "write"),
    ]);
    expect(perms.find((p) => p.key === "actions")?.level).toBe("write");
  });

  test("never downgrades the minimum floor to a read-only power", () => {
    // A `contents:read` power must not clobber the required `contents:write` floor.
    const perms = permissionsForCapabilities([cap("contents:read", "read")]);
    expect(perms.find((p) => p.key === "contents")?.level).toBe("write");
  });

  test("returns permissions sorted by key (stable UI order)", () => {
    const perms = permissionsForCapabilities([
      cap("actions:write", "write"),
      cap("checks:read", "read"),
    ]);
    const keys = perms.map((p) => p.key);
    expect(keys).toEqual([...keys].sort());
  });
});

describe("isMinimumPermission / permissionLabel", () => {
  test("flags only the floor permissions as minimum", () => {
    expect(isMinimumPermission("contents")).toBe(true);
    expect(isMinimumPermission("pull_requests")).toBe(true);
    expect(isMinimumPermission("actions")).toBe(false);
  });

  test("labels pull_requests back to its human resource name", () => {
    expect(permissionLabel("pull_requests")).toBe("Pull requests");
    expect(permissionLabel("metadata")).toBe("Repository metadata");
  });
});

describe("githubWebhookUrl", () => {
  test("uses the /api/v1/integrations/github/events convention", () => {
    expect(githubWebhookUrl("https://engrams.example.com")).toBe(
      "https://engrams.example.com/api/v1/integrations/github/events",
    );
  });
});

describe("buildGithubManifest", () => {
  const origin = "https://engrams.example.com";
  const parse = () =>
    JSON.parse(
      buildGithubManifest({
        origin,
        permissions: permissionsForCapabilities([cap("pulls:write", "write")]),
        events: WEBHOOK_EVENTS,
      }),
    ) as Record<string, any>;

  test("names the app engrams and points the webhook at the events route", () => {
    const m = parse();
    expect(m.name).toBe("engrams");
    expect(m.hook_attributes.url).toBe(githubWebhookUrl(origin));
    expect(m.hook_attributes.active).toBe(true);
  });

  test("carries the derived permissions as default_permissions", () => {
    const m = parse();
    expect(m.default_permissions.metadata).toBe("read");
    expect(m.default_permissions.contents).toBe("write");
    expect(m.default_permissions.pull_requests).toBe("write");
  });

  test("subscribes the review-driven events", () => {
    const m = parse();
    expect(m.default_events).toContain("pull_request");
    expect(m.default_events).toContain("pull_request_review");
  });

  test("emits pretty-printed, valid JSON", () => {
    const out = buildGithubManifest({
      origin,
      permissions: MINIMUM_PERMISSIONS,
      events: WEBHOOK_EVENTS,
    });
    expect(out).toContain("\n");
    expect(() => JSON.parse(out)).not.toThrow();
  });
});
