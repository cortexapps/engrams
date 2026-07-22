import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";

import { ConnectorSchema } from "@/gen/engram/app/v1/integration_pb";
import { automationErrorField, rawVariables, webhookConnectorHints } from "@/lib/automations";

describe("automation UI helpers", () => {
  it("offers only connectors with a valid webhook facet", () => {
    const connectors = [
      create(ConnectorSchema, {
        provider: "github",
        configJson: JSON.stringify({
          display: { name: "GitHub" },
          webhook: { verificationScheme: "github_hmac_sha256" },
        }),
      }),
      create(ConnectorSchema, {
        provider: "stripe",
        configJson: JSON.stringify({ display: { name: "Stripe" } }),
      }),
      create(ConnectorSchema, { provider: "broken", configJson: "{" }),
    ];

    expect(webhookConnectorHints(connectors)).toEqual([
      {
        provider: "github",
        name: "GitHub",
        verificationScheme: "github_hmac_sha256",
      },
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
    expect(automationErrorField("profile has port_exposures and cannot be used")).toBe("profile");
    expect(automationErrorField("unknown filter in title template")).toBe("titleTemplate");
  });
});
