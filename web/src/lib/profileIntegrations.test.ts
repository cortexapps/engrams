import { describe, expect, it } from "vitest";

import { defaultCapabilitiesForGrants } from "./profileIntegrations";

describe("defaultCapabilitiesForGrants", () => {
  it("projects default connector grants and ignores other named connections", () => {
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
          {
            connectionId: "gcp-prod",
            operation: "compute.instances.get",
            resourceConstraints: [],
          },
        ],
        [{ provider: "github", defaultConnectionId: "connection-github" }],
      ),
    ).toEqual([
      "github:issues:read",
      "github:repos:read@cortexapps/engrams",
      "github:repos:read@cortexapps/cortex",
    ]);
  });
});
