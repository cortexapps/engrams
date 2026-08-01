import { describe, expect, it } from "vitest";

import { legacyCapabilitiesForGrants } from "./profileIntegrations";

describe("legacyCapabilitiesForGrants", () => {
  it("projects migrated connector grants and ignores named cloud connections", () => {
    expect(
      legacyCapabilitiesForGrants([
        {
          connectionId: "legacy:github",
          operation: "issues:read",
          resourceConstraints: [],
        },
        {
          connectionId: "legacy:github",
          operation: "repos:read",
          resourceConstraints: ["cortexapps/engrams", "cortexapps/cortex"],
        },
        {
          connectionId: "gcp-prod",
          operation: "compute.instances.get",
          resourceConstraints: [],
        },
      ]),
    ).toEqual([
      "github:issues:read",
      "github:repos:read@cortexapps/engrams",
      "github:repos:read@cortexapps/cortex",
    ]);
  });
});
