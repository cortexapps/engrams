import { describe, expect, test } from "bun:test";
import { Code } from "@connectrpc/connect";

import type { IntegrationPolicyJson } from "../connectors/registry.ts";
import type { IntegrationConnectionStore } from "../db/integration-connections.ts";
import { appendGooglePolicy } from "../integrations/google-policy.ts";
import { resolveIntegrationGrants } from "../integrations/grants.ts";
import type { ResolvedIntegrationGrant } from "../integrations/grants.ts";

function policy(): IntegrationPolicyJson {
  return { network: { default: "deny", allow_hosts: [], allow_host_patterns: [] }, secrets: [], injects: [], observes: [] };
}

function grant(operation: string, resourceConstraints: string[] = []): ResolvedIntegrationGrant {
  return {
    grant: { connectionId: "connection-1", operation, resourceConstraints },
    connection: {
      id: "connection-1",
      alias: "prod-readonly",
      provider: "gcp",
      displayName: "Production read only",
      config: {
        workloadIdentityProvider: "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/engrams/providers/oidc",
        serviceAccountEmail: "reader@customer.iam.gserviceaccount.com",
        endpoints: ["compute.googleapis.com", "logging.googleapis.com"],
      },
      enabled: true,
      testedAt: new Date(0),
      createdAt: new Date(0),
      updatedAt: new Date(0),
    },
  };
}

describe("Google egress policy", () => {
  test("credential-producing operations cannot be granted", async () => {
    const connection = grant("compute.instances.get").connection;
    const store: IntegrationConnectionStore = {
      async list() {
        return [connection];
      },
      async get() {
        return connection;
      },
      async create() {
        return connection;
      },
      async update() {
        return connection;
      },
      async delete() {
        return true;
      },
      async markTested() {
        return connection;
      },
      async setEnabled() {
        return connection;
      },
      async ensureLegacy() {},
    };
    await expect(resolveIntegrationGrants(
      [{
        connectionId: connection.id,
        operation: "iam.generateaccesstoken",
        resourceConstraints: [],
      }],
      store,
    )).rejects.toMatchObject({ code: Code.InvalidArgument });
  });

  test("compiles an exact operation and constrained API path", () => {
    const output = policy();
    appendGooglePolicy(output, [grant("compute.instances.start", [
      "/compute/v1/projects/prod/zones/us-central1-a/instances/engram-dev/start",
    ])]);
    expect(output.network.allow_hosts).toEqual(["compute.googleapis.com"]);
    expect(output.injects[0]).toMatchObject({
      hosts: ["compute.googleapis.com"],
      methods: ["POST"],
      path_globs: [
        "segment:/compute/v1/projects/prod/zones/us-central1-a/instances/engram-dev/start",
      ],
      mint_provider: "gcp|connection-1|compute.instances.start|compute.googleapis.com",
    });
  });

  test("rejects constraints that cannot be enforced at the proxy boundary", () => {
    expect(() => appendGooglePolicy(policy(), [
      grant("logging.entries.list", ["/projects/prod"]),
    ])).toThrow(/cannot be enforced/);
    expect(() => appendGooglePolicy(policy(), [
      grant("compute.instances.stop", ["/compute/v1/projects/other"]),
    ])).toThrow(/not a valid/);
  });

  test("curates the Logging REST and gRPC methods", () => {
    const output = policy();
    appendGooglePolicy(output, [grant("logging.entries.list")]);
    expect(output.injects[0]).toMatchObject({
      hosts: ["logging.googleapis.com"],
      methods: ["POST"],
      path_globs: [
        "segment:/v2/entries:list",
        "segment:/google.logging.v2.LoggingServiceV2/ListLogEntries",
      ],
    });
  });

  test("broad API access remains limited to configured googleapis hosts", () => {
    const output = policy();
    const resolved = grant("api.call", ["/v1/projects/prod/*"]);
    resolved.connection.config.endpoints = ["compute.googleapis.com", "cluster.example.com"];
    appendGooglePolicy(output, [resolved]);
    expect(output.network.allow_hosts).toEqual(["compute.googleapis.com"]);
    expect(output.injects).toHaveLength(1);
  });

  test("GKE access uses only explicit non-Google API endpoints", () => {
    const output = policy();
    const resolved = grant("gke.api.call", ["/api/v1/namespaces/default/pods"]);
    resolved.connection.config.endpoints = ["container.googleapis.com", "cluster.example.com"];
    appendGooglePolicy(output, [resolved]);
    expect(output.network.allow_hosts).toEqual(["cluster.example.com"]);
    expect(output.injects[0]).toMatchObject({
      hosts: ["cluster.example.com"],
      path_globs: ["segment:/api/v1/namespaces/default/pods"],
    });
  });

  test("curated operations require their exact connection endpoint", () => {
    const resolved = grant("iap.tunnel");
    expect(() => appendGooglePolicy(policy(), [resolved])).toThrow(
      /does not enable tunnel\.cloudproxy\.app/,
    );
  });
});
