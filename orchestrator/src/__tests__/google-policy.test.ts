import { describe, expect, test } from "bun:test";
import { Code } from "@connectrpc/connect";

import type { IntegrationPolicyJson } from "../connectors/registry.ts";
import type { IntegrationConnectionStore } from "../db/integration-connections.ts";
import {
  appendGooglePolicy,
  validateGoogleGrants,
  CURATED_GOOGLE_OPERATIONS,
  FORBIDDEN_GOOGLE_OPERATIONS,
  GOOGLE_PASSTHROUGH_OPERATIONS,
  CLOUD_SQL_POSTGRES_CONNECT,
} from "../integrations/google-policy.ts";
import { resolveIntegrationGrants } from "../integrations/grants.ts";
import type { ResolvedIntegrationGrant } from "../integrations/grants.ts";

function policy(): IntegrationPolicyJson {
  return { network: { default: "deny", allow_hosts: [], allow_host_patterns: [] }, secrets: [], injects: [], observes: [], metadata_flavor: null, cloud_sql_tunnels: [] };
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
  test("compiles one exact Cloud SQL tunnel without a credential value", () => {
    const resolved = grant(CLOUD_SQL_POSTGRES_CONNECT, [], []);
    resolved.connection.config.cloudSqlPostgresInstance = "customer:us-central1:prod";
    const output = policy();

    appendGooglePolicy(output, [resolved]);

    expect(output.injects).toEqual([]);
    expect(output.cloud_sql_tunnels).toEqual([{
      instance: "customer:us-central1:prod",
      database_user: "reader@customer.iam",
      mint_source: {
        connection: { connection_id: "connection-1", provider: "gcp" },
      },
    }]);
    expect(JSON.stringify(output)).not.toContain("ya29");
  });

  test("Cloud SQL requires a configured instance and rejects grant constraints", () => {
    expect(() => appendGooglePolicy(policy(), [grant(CLOUD_SQL_POSTGRES_CONNECT, [], [])]))
      .toThrow(/has no Cloud SQL PostgreSQL instance/);
    expect(() => validateGoogleGrants([grant(
      CLOUD_SQL_POSTGRES_CONNECT,
      ["projects/customer/instances/prod"],
      [],
    )])).toThrow(/does not accept resource constraints/);
  });
  test("credential-producing operations cannot be granted", async () => {
    const connection = grant("compute.instances.get").connection;
    const store: IntegrationConnectionStore = {
      async list() {
        return [connection];
      },
      async get() {
        return connection;
      },
      async getMany(ids) {
        return ids.map(() => connection);
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
    // S6: reachability rides the mint inject only. The host never enters the
    // generic network allow-list, so a failed boot-time mint fails closed.
    expect(output.network.allow_hosts).toEqual([]);
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

  test("curates the Logging REST and gRPC methods as separate entries", () => {
    const output = policy();
    appendGooglePolicy(output, [grant("logging.entries.list")]);
    expect(output.injects).toEqual([
      expect.objectContaining({
        hosts: ["logging.googleapis.com"],
        methods: ["POST"],
        path_globs: ["segment-path:/v2/entries:list"],
      }),
      expect.objectContaining({
        hosts: ["logging.googleapis.com"],
        methods: ["POST"],
        path_globs: ["segment-path:/google.logging.v2.LoggingServiceV2/ListLogEntries"],
      }),
    ]);
  });

  test("curates Monitoring reads to GET-only REST entries and POST-only gRPC entries", () => {
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
        methods: ["GET"],
        path_globs: ["segment-path:/v3/projects/*/metricDescriptors"],
      }),
      expect.objectContaining({
        hosts: ["monitoring.googleapis.com"],
        methods: ["POST"],
        path_globs: ["segment-path:/google.monitoring.v3.MetricService/ListMetricDescriptors"],
      }),
      expect.objectContaining({
        hosts: ["monitoring.googleapis.com"],
        methods: ["GET"],
        path_globs: ["segment-path:/v3/projects/*/timeSeries"],
      }),
      expect.objectContaining({
        hosts: ["monitoring.googleapis.com"],
        methods: ["POST"],
        path_globs: ["segment-path:/google.monitoring.v3.MetricService/ListTimeSeries"],
      }),
    ]);
  });

  test("a curated read grant never allows the write verb on a REST resource path", () => {
    // The proxy matches methods and paths independently inside one entry.
    // `POST /v3/projects/*/timeSeries` is `timeSeries.create` — a WRITE. No
    // entry compiled from a read grant may pair POST with a REST resource path.
    const output = policy();
    appendGooglePolicy(output, [
      grant("monitoring.timeseries.list", [], ["monitoring.googleapis.com"]),
    ]);
    const restPath = "segment-path:/v3/projects/*/timeSeries";
    for (const entry of output.injects) {
      const allowsPost = entry.methods.length === 0 || entry.methods.includes("POST");
      const matchesRestPath = entry.path_globs.includes(restPath);
      expect(allowsPost && matchesRestPath).toBe(false);
    }
    // The read stays reachable: GET on the REST path, POST on the gRPC path only.
    expect(output.injects.some((entry) =>
      entry.methods.length === 1 && entry.methods[0] === "GET" &&
      entry.path_globs.includes(restPath)
    )).toBe(true);
    expect(output.injects.some((entry) =>
      entry.methods.length === 1 && entry.methods[0] === "POST" &&
      entry.path_globs.length === 1 &&
      entry.path_globs[0] === "segment-path:/google.monitoring.v3.MetricService/ListTimeSeries"
    )).toBe(true);
  });

  test("a resource constraint narrows the REST paths and drops the gRPC surface", () => {
    // A path constraint cannot scope a gRPC request body, so a constrained
    // grant compiles the REST entry only.
    const output = policy();
    appendGooglePolicy(output, [grant(
      "monitoring.timeseries.list",
      ["/v3/projects/prod/timeSeries"],
      ["monitoring.googleapis.com"],
    )]);
    expect(output.injects).toEqual([
      expect.objectContaining({
        methods: ["GET"],
        path_globs: ["segment-path:/v3/projects/prod/timeSeries"],
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
    expect(output.network.allow_hosts).toEqual([]);
    expect(output.injects).toHaveLength(1);
    expect(output.injects[0]!.hosts).toEqual(["compute.googleapis.com"]);
  });

  test("GKE access uses only explicit non-Google API endpoints", () => {
    const output = policy();
    const resolved = grant("gke.api.call", ["/api/v1/namespaces/default/pods"]);
    resolved.connection.config.endpoints = ["container.googleapis.com", "cluster.example.com"];
    appendGooglePolicy(output, [resolved]);
    expect(output.network.allow_hosts).toEqual([]);
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

  // O11: matcher overlap is STRUCTURAL, not string equality. A glob and a
  // constrained exact path match the same request, and the proxy would
  // credential it with whichever connection's entry it finds first.
  test("rejects two connections whose matchers can select the same request", () => {
    const broad = grant("monitoring.timeseries.list", [], ["monitoring.googleapis.com"]);
    const scoped = grant(
      "monitoring.timeseries.list",
      ["/v3/projects/prod/timeSeries"],
      ["monitoring.googleapis.com"],
    );
    scoped.connection = { ...scoped.connection, id: "connection-2", alias: "prod-scoped" };
    scoped.grant = { ...scoped.grant, connectionId: "connection-2" };

    expect(() => appendGooglePolicy(policy(), [broad, scoped])).toThrow(
      /conflicting credentials for monitoring\.googleapis\.com/,
    );
  });

  test("allows two connections with disjoint constrained paths on one host", () => {
    const output = policy();
    const alpha = grant(
      "monitoring.timeseries.list",
      ["/v3/projects/alpha/timeSeries"],
      ["monitoring.googleapis.com"],
    );
    const beta = grant(
      "monitoring.timeseries.list",
      ["/v3/projects/beta/timeSeries"],
      ["monitoring.googleapis.com"],
    );
    beta.connection = { ...beta.connection, id: "connection-2", alias: "beta-scoped" };
    beta.grant = { ...beta.grant, connectionId: "connection-2" };

    expect(() => appendGooglePolicy(output, [alpha, beta])).not.toThrow();
    expect(output.injects).toHaveLength(2);
  });

  // O9: profile save validates SHAPE only; connection STATE gates session-create.
  test("validateGoogleGrants accepts a disabled connection that session-create rejects", () => {
    const resolved = grant("compute.instances.get");
    resolved.connection.enabled = false;

    expect(() => validateGoogleGrants([resolved])).not.toThrow();
    expect(() => appendGooglePolicy(policy(), [resolved])).toThrow(/is disabled/);
  });

  test("validateGoogleGrants ignores endpoint membership (connection state)", () => {
    // `iap.tunnel` needs tunnel.cloudproxy.app, which this connection does not
    // enable. Editing endpoints auto-disables a connection; the profile that
    // grants it must still save.
    const resolved = grant("iap.tunnel");
    expect(() => validateGoogleGrants([resolved])).not.toThrow();
  });

  test("validateGoogleGrants rejects unknown operations and invalid constraints", () => {
    expect(() => validateGoogleGrants([grant("monitoring.timeseries.write")])).toThrow(
      /unknown Google Cloud operation/,
    );
    expect(() => validateGoogleGrants([
      grant("compute.instances.stop", ["/compute/v1/projects/other"]),
    ])).toThrow(/not a valid/);
    expect(() => validateGoogleGrants([
      grant("logging.entries.list", ["/projects/prod"]),
    ])).toThrow(/cannot be enforced/);
  });
});

describe("curated Google operation table", () => {
  // O13: the surfaces and the constraint validators live in ONE record. This
  // test pins the remaining cross-field consistency so an edit to one half of
  // an entry cannot silently strand the other half.
  test("every entry is complete and its validator accepts its own paths", () => {
    const names = Object.keys(CURATED_GOOGLE_OPERATIONS);
    expect(names.length).toBeGreaterThan(0);
    for (const [name, operation] of Object.entries(CURATED_GOOGLE_OPERATIONS)) {
      expect(name).toMatch(/^[a-z][a-z0-9_.-]*$/);
      expect(FORBIDDEN_GOOGLE_OPERATIONS.has(name)).toBe(false);
      expect((GOOGLE_PASSTHROUGH_OPERATIONS as readonly string[]).includes(name)).toBe(false);
      expect(operation.host.length).toBeGreaterThan(0);
      expect(operation.rest.methods.length).toBeGreaterThan(0);
      expect(operation.rest.paths.length).toBeGreaterThan(0);
      for (const path of [...operation.rest.paths, ...operation.grpcPaths]) {
        expect(path.startsWith("/")).toBe(true);
      }
      if (operation.constraint === "none") continue;
      for (const glob of operation.rest.paths) {
        // A constraint is a concrete instance of the operation's own glob:
        // the validator must accept the glob with segments filled in, and
        // must reject a path that escapes below it.
        const samples = [glob.replaceAll("*", "example"), glob.replaceAll("*", "")];
        expect(samples.some((sample) => (operation.constraint as RegExp).test(sample))).toBe(true);
        expect(operation.constraint.test(`${glob.replaceAll("*", "example")}/escape`)).toBe(false);
      }
    }
  });
});
