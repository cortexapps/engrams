import { reviewCatalog } from "../mcp/catalog.ts";
import type { ProfileIntegrationGrant } from "../db/schema.ts";
export const operatorPublication = {
  generation: 1,
  identity: `sha256:${"b".repeat(64)}`,
  deploymentIdentity: `sha256:${"c".repeat(64)}`,
  networkIdentity: `sha256:${"d".repeat(64)}`,
  trustIdentity: `sha256:${"e".repeat(64)}`,
  caContentDigests: { control: `sha256:${"f".repeat(64)}` },
};
export const schema = {
  $schema: "https://json-schema.org/draft/2020-12/schema",
  $defs: { name: { type: "string" } },
  type: "object",
  properties: { name: { $ref: "#/$defs/name" } },
};
export const bindingGrant: ProfileIntegrationGrant = {
  connectionId: "connection-a",
  operation: "invoke",
  credentialScope: "org",
  resourceConstraints: [],
  mcp: {
    upstreamName: "Document.Search",
    automationWrite: false,
    unrestricted: true,
    argumentConstraintDocuments: [],
  },
};
export function bindingAuthority() {
  return {
    profileId: "profile-a",
    profileGrants: [structuredClone(bindingGrant)],
    connections: [
      {
        id: "connection-a",
        catalogId: "catalog-a-1",
        alias: "docs",
        enabled: true,
        config: {
          endpoint: "https://tools.example.test/mcp",
          zone: "public",
          auth: { kind: "api_key" as const, secretRef: "mcp.key" },
        },
        state: {
          configRevision: 1,
          authGeneration: 1,
          catalogRevision: 1,
          credentialIdentity: "identity-a",
          revoked: false,
        },
        tools: reviewCatalog([
          {
            upstreamName: "Document.Search",
            description: "Search",
            inputSchema: schema,
            classification: "read",
            enabled: true,
          },
        ]),
      },
    ],
    operatorPolicies: [
      {
        publication: structuredClone(operatorPublication),
        zoneId: "public",
        policyRevision: "1",
        policyDigest: `sha256:${"a".repeat(64)}`,
        authorities: ["tools.example.test"],
        aliases: ["alias.example.test"],
      },
    ],
  };
}
