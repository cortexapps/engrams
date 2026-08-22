import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";

import { ConnectorSchema } from "@/gen/engram/app/v1/integration_pb";
import { relativeTime, webhookConnectorHints } from "@/lib/automations";

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
});
