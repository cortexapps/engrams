import { describe, expect, test } from "bun:test";
import { Code } from "@connectrpc/connect";

import type { IntegrationPolicyJson } from "../connectors/registry.ts";
import type { IntegrationConnectionStore } from "../db/integration-connections.ts";
import { appendGooglePolicy } from "../integrations/google-policy.ts";
import { resolveIntegrationGrants } from "../integrations/grants.ts";
import type { ResolvedIntegrationGrant } from "../integrations/grants.ts";

function policy(): IntegrationPolicyJson {
  return { network: { default: "deny", allow_hosts: [], allow_host_patterns: [] }, secrets: [], injects: [], observes: [], google_adc: false };
}

function grant(
  operation: string,
  resourceConstraints: string[] = [],
  endpoints = ["compute.googleapis.com", "logging.googleapis.com"],
): ResolvedIntegrationGrant {
  return {
    grant: { connectionId: "connection-1", operation, resourceConstraints },
    connection: {
      id: "connection-1",
      alias: "prod-readonly",
      provider: "gcp",
      displayName: "Production read only",
      isDefault: false,
      config: {
        workloadIdentityProvider: "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/engrams/providers/oidc",
        serviceAccountEmail: "reader@customer.iam.gserviceaccount.com",
        endpoints,
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
      async getDefault() {
        return null;
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
      async ensureDefault() {
        return connection;
      },
    };
    await expect(resolveIntegrationGrants(
      [{
        connectionId: connection.id,
        operation: "iam.generateaccesstoken",
        resourceConstraints: [],
      }],
      store,
    )).rejects.toMatchObject({ code: Code.InvalidArgument });

    const observerReads = await resolveIntegrationGrants(
      [
        {
          connectionId: connection.id,
          operation: "monitoring.metricdescriptors.list",
          resourceConstraints: [],
        },
        {
          connectionId: connection.id,
          operation: "monitoring.timeseries.list",
          resourceConstraints: [],
        },
      ],
      store,
    );
    expect(observerReads.map(({ grant: resolved }) => resolved.operation)).toEqual([
      "monitoring.metricdescriptors.list",
      "monitoring.timeseries.list",
    ]);
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
        "segment-path:/compute/v1/projects/prod/zones/us-central1-a/instances/engram-dev/start",
      ],
      mint_source: { connection: { connection_id: "connection-1" } },
    });
  });

  test("rejects constraints that cannot be enforced at the proxy boundary", () => {
    expect(() => appendGooglePolicy(policy(), [
      grant("logging.entries.list", ["/projects/prod"]),
    ])).toThrow(/cannot be enforced/);
    expect(() => appendGooglePolicy(policy(), [
      grant("compute.instances.stop", ["/compute/v1/projects/other"]),
    ])).toThrow(/not a valid/);
    expect(() => appendGooglePolicy(policy(), [
      grant("monitoring.timeseries.list", [
        "/v3/projects/prod/timeSeries?filter=metric.type%3Dx",
      ], ["monitoring.googleapis.com"]),
    ])).toThrow(/not a valid/);
  });

  test("curates the Logging REST and gRPC methods", () => {
    const output = policy();
    appendGooglePolicy(output, [grant("logging.entries.list")]);
    expect(output.injects[0]).toMatchObject({
      hosts: ["logging.googleapis.com"],
      methods: ["POST"],
      path_globs: [
        "segment-path:/v2/entries:list",
        "segment-path:/google.logging.v2.LoggingServiceV2/ListLogEntries",
      ],
    });
  });

  test("curates Monitoring reads to list-only REST and gRPC methods", () => {
    const output = policy();
    const descriptors = grant(
      "monitoring.metricdescriptors.list",
      [],
      ["monitoring.googleapis.com"],
    );
    const timeSeries = grant("monitoring.timeseries.list", [], ["monitoring.googleapis.com"]);

    appendGooglePolicy(output, [descriptors, timeSeries]);

    expect(output.injects).toEqual([
      expect.objectContaining({
        hosts: ["monitoring.googleapis.com"],
        methods: ["GET", "POST"],
        path_globs: [
          "segment-path:/v3/projects/*/metricDescriptors",
          "segment-path:/google.monitoring.v3.MetricService/ListMetricDescriptors",
        ],
      }),
      expect.objectContaining({
        hosts: ["monitoring.googleapis.com"],
        methods: ["GET", "POST"],
        path_globs: [
          "segment-path:/v3/projects/*/timeSeries",
          "segment-path:/google.monitoring.v3.MetricService/ListTimeSeries",
        ],
      }),
    ]);
  });

  test("separates Trace list and detail paths", () => {
    const output = policy();
    const list = grant("trace.traces.list", [], ["cloudtrace.googleapis.com"]);
    const get = grant("trace.traces.get", [], ["cloudtrace.googleapis.com"]);

    appendGooglePolicy(output, [list, get]);

    expect(output.injects.map((entry) => entry.path_globs)).toEqual([
      ["segment-path:/v1/projects/*/traces"],
      ["segment-path:/v1/projects/*/traces/*"],
    ]);
  });

  test("validates project-scoped observer constraints", () => {
    const monitoring = grant(
      "monitoring.timeseries.list",
      ["/v3/projects/cortex-internal-tooling/timeSeries"],
      ["monitoring.googleapis.com"],
    );
    expect(() => appendGooglePolicy(policy(), [monitoring])).not.toThrow();

    const trace = grant(
      "trace.traces.get",
      ["/v1/projects/cortex-internal-tooling/traces/trace-1"],
      ["cloudtrace.googleapis.com"],
    );
    expect(() => appendGooglePolicy(policy(), [trace])).not.toThrow();

    const invalid = grant(
      "monitoring.timeseries.list",
      ["/v3/projects/cortex-internal-tooling/timeSeries/credential-producing-action"],
      ["monitoring.googleapis.com"],
    );
    expect(() => appendGooglePolicy(policy(), [invalid])).toThrow(/not a valid/);
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
      path_globs: ["segment-path:/api/v1/namespaces/default/pods"],
    });
  });

  test("curated operations require their exact connection endpoint", () => {
    const resolved = grant("iap.tunnel");
    expect(() => appendGooglePolicy(policy(), [resolved])).toThrow(
      /does not enable tunnel\.cloudproxy\.app/,
    );
  });
});
