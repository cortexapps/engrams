import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";

import { ConnectorSchema } from "@/gen/engram/app/v1/integration_pb";
import {
  automationErrorField,
  definitionFromDraft,
  draftFromDefinition,
  rawVariables,
  relativeTime,
  singleBlockPreview,
  webhookConnectorHints,
} from "@/lib/automations";

describe("automation UI helpers", () => {
  it("relativeTime is symmetric for past and future (next-fire renders 'in Xm', never 'just now')", () => {
    const now = new Date("2026-08-22T12:00:00Z");
    expect(relativeTime("2026-08-22T11:56:00Z", now)).toBe("4m ago");
    expect(relativeTime("2026-08-22T12:04:00Z", now)).toBe("in 4m");
    expect(relativeTime("2026-08-22T15:00:00Z", now)).toBe("in 3h");
    expect(relativeTime("2026-08-24T12:00:00Z", now)).toBe("in 2d");
    expect(relativeTime("2026-08-22T12:00:30Z", now)).toBe("any moment");
    expect(relativeTime("2026-08-22T11:59:30Z", now)).toBe("just now");
    expect(relativeTime(undefined, now)).toBe("never");
  });

  it("offers only connectors with a webhook facet and no integration ingress", () => {
    const connectors = [
      create(ConnectorSchema, {
        provider: "github",
        configJson: JSON.stringify({
          display: { name: "GitHub" },
          webhook: {
            verificationScheme: "github_hmac_sha256",
            ingress: { scheme: "github_hmac_sha256", secretRef: "github.webhook_secret" },
          },
        }),
      }),
      create(ConnectorSchema, {
        provider: "pagerduty",
        configJson: JSON.stringify({
          display: { name: "PagerDuty" },
          webhook: { verificationScheme: "generic_hmac_sha256" },
        }),
      }),
      create(ConnectorSchema, {
        provider: "stripe",
        configJson: JSON.stringify({ display: { name: "Stripe" } }),
      }),
      create(ConnectorSchema, { provider: "broken", configJson: "{" }),
    ];

    // GitHub owns an ingress: its events are integration triggers, not hints.
    expect(webhookConnectorHints(connectors)).toEqual([
      { provider: "pagerduty", name: "PagerDuty" },
    ]);
  });

  it("derives bounded raw variable paths from a sample", () => {
    expect(
      rawVariables(JSON.stringify({ issue: { title: "Bug", labels: [{ name: "urgent" }] } })),
    ).toEqual([
      { path: "issue.title", depth: 2 },
      { path: "issue.labels.0.name", depth: 4 },
    ]);
    expect(rawVariables("not-json")).toEqual([]);
  });

  it("places server validation messages beside actionable fields", () => {
    expect(automationErrorField("invalid cron expression")).toBe("schedule");
    expect(automationErrorField("action profile_id is not an active profile")).toBe("profile");
    expect(automationErrorField("unknown filter in title template")).toBe("titleTemplate");
  });

  it("round-trips a single-block draft through definition_json", () => {
    const draft = {
      triggerKind: "webhook" as const,
      schedule: "",
      timezone: "",
      registrationId: "team-hook",
      events: ["push"],
      profileId: "pf1",
      promptTemplate: "Hi ${{ event.raw.ref }}",
      titleTemplate: "",
      includeEventContext: true,
      harness: "codex",
      model: "gpt",
    };
    const json = definitionFromDraft(draft);
    const parsed = JSON.parse(json) as {
      blocks: Array<{ id: string; config: Record<string, unknown> }>;
    };
    expect(parsed.blocks).toHaveLength(1);
    expect(parsed.blocks[0]!.config).not.toHaveProperty("titleTemplate");
    expect(draftFromDefinition(json)).toEqual(draft);
  });

  it("refuses to draft a multi-block or built-in graph", () => {
    const multi = JSON.stringify({
      engine: 1,
      trigger: { kind: "manual" },
      blocks: [
        { id: "a", type: "filter", config: {} },
        { id: "b", type: "create_session", config: { profileId: "p", promptTemplate: "x" } },
      ],
      inputsSchema: [],
      settings: { endSessionsOnFinish: false },
    });
    expect(draftFromDefinition(multi)).toBeNull();
    expect(draftFromDefinition("not json")).toBeNull();
  });

  it("lifts the single block's rendered prompt and title out of a v2 preview", () => {
    expect(
      singleBlockPreview([
        { blockId: "other", renderedJson: "{}" },
        {
          blockId: "create_session",
          renderedJson: JSON.stringify({ promptTemplate: "Rendered", titleTemplate: "T" }),
        },
      ]),
    ).toEqual({ renderedPrompt: "Rendered", renderedTitle: "T" });
    expect(singleBlockPreview([])).toEqual({});
  });
});
