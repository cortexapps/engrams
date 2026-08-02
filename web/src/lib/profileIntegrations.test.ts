import { describe, expect, it } from "vitest";

import { defaultCapabilitiesForGrants } from "./profileIntegrations";

const GITHUB_VIEW = {
  provider: "github",
  defaultConnectionId: "connection-github",
  capabilities: [{ action: "issues:read" }, { action: "repos:read" }],
};

const GOOGLE_VIEW = {
  provider: "gcp",
  defaultConnectionId: "",
  connectionModel: "named" as const,
  capabilities: [{ action: "compute.instances.get" }, { action: "logging.entries.list" }],
};

describe("defaultCapabilitiesForGrants", () => {
  it("projects default connector grants with their resource constraints", () => {
    expect(
      defaultCapabilitiesForGrants(
        [
          {
            connectionId: "connection-github",
            operation: "issues:read",
            resourceConstraints: [],
          },
          {
            connectionId: "connection-github",
            operation: "repos:read",
            resourceConstraints: ["cortexapps/engrams", "cortexapps/cortex"],
          },
        ],
        [GITHUB_VIEW, GOOGLE_VIEW],
      ),
    ).toEqual([
      "github:issues:read",
      "github:repos:read@cortexapps/engrams",
      "github:repos:read@cortexapps/cortex",
    ]);
  });

  // web-M1: named-connection grants must reach the policy preview. Before,
  // a Google grant mapped through defaultConnectionId="" and vanished — the
  // rail said "No powers granted" on a profile that can call Compute.
  it("attributes named-connection grants to the provider that offers the operation", () => {
    expect(
      defaultCapabilitiesForGrants(
        [
          {
            connectionId: "gcp-prod",
            operation: "compute.instances.get",
            resourceConstraints: [],
          },
          {
            connectionId: "gcp-staging",
            operation: "compute.instances.get",
            resourceConstraints: [],
          },
          {
            connectionId: "gcp-prod",
            operation: "logging.entries.list",
            resourceConstraints: [],
          },
        ],
        [GITHUB_VIEW, GOOGLE_VIEW],
      ),
    ).toEqual(["gcp:compute.instances.get", "gcp:logging.entries.list"]);
  });

  it("drops grants that match no provider", () => {
    expect(
      defaultCapabilitiesForGrants(
        [{ connectionId: "unknown", operation: "not.an.op", resourceConstraints: [] }],
        [GITHUB_VIEW, GOOGLE_VIEW],
      ),
    ).toEqual([]);
  });
});
