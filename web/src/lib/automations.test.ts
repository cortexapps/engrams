import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";

import { ConnectorSchema } from "@/gen/engram/app/v1/integration_pb";
import { webhookConnectorHints } from "@/lib/automations";

describe("automation UI helpers", () => {
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
