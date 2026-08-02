/** Curated Google API request policy compilation (ADR 0109). */

import { ConnectError, Code } from "@connectrpc/connect";

import type { IntegrationInjectJson, IntegrationPolicyJson } from "../connectors/registry.ts";
import { assertGoogleCloudConfig } from "./google-wif.ts";
import type { ResolvedIntegrationGrant } from "./grants.ts";

interface GoogleOperationPolicy {
  host: string;
  methods: string[];
  paths: string[];
}

function constrainedPaths(operation: string, defaults: string[], constraints: readonly string[]): string[] {
  if (constraints.length === 0) return defaults;
  if (operation === "logging.entries.list") {
    throw new ConnectError(
      "logging.entries.list resource constraints cannot be enforced at the HTTP path boundary",
      Code.InvalidArgument,
    );
  }
  const validators: Record<string, RegExp> = {
    "compute.instances.get": /^\/compute\/v1\/projects\/[^/]+\/zones\/[^/]+\/instances\/[^/?]+(?:\?.*)?$/,
    "compute.instances.start": /^\/compute\/v1\/projects\/[^/]+\/zones\/[^/]+\/instances\/[^/?]+\/start(?:\?.*)?$/,
    "compute.instances.stop": /^\/compute\/v1\/projects\/[^/]+\/zones\/[^/]+\/instances\/[^/?]+\/stop(?:\?.*)?$/,
    "trace.traces.list": /^\/v1\/projects\/[^/?]+\/traces(?:\?.*)?$/,
    "container.clusters.get": /^\/v1\/projects\/[^/]+\/locations\/[^/]+\/clusters\/[^/?]+(?:\?.*)?$/,
    "iap.tunnel": /^\/v4\/connect(?:\?.*)?$/,
  };
  const validator = validators[operation];
  for (const constraint of constraints) {
    if (
      constraint.length > 2048 ||
      !constraint.startsWith("/") ||
      constraint.includes("://") ||
      constraint.includes("..") ||
      (validator !== undefined && !validator.test(constraint))
    ) {
      throw new ConnectError(
        `resource constraint "${constraint}" is not a valid ${operation} API path`,
        Code.InvalidArgument,
      );
    }
  }
  return [...new Set(constraints)];
}

function matchersOverlap(a: IntegrationInjectJson, b: IntegrationInjectJson): boolean {
  const methodsOverlap = a.methods.length === 0 || b.methods.length === 0 ||
    a.methods.some((method) => b.methods.includes(method));
  // Exact equality catches the unsafe common case. Empty means every path.
  const pathsOverlap = a.path_globs.length === 0 || b.path_globs.length === 0 ||
    a.path_globs.some((path) => b.path_globs.includes(path));
  return methodsOverlap && pathsOverlap;
}

const CURATED_GOOGLE_OPERATIONS: Record<string, GoogleOperationPolicy> = {
  "compute.instances.get": {
    host: "compute.googleapis.com",
    methods: ["GET"],
    paths: ["/compute/v1/projects/*/zones/*/instances/*"],
  },
  "compute.instances.start": {
    host: "compute.googleapis.com",
    methods: ["POST"],
    paths: ["/compute/v1/projects/*/zones/*/instances/*/start"],
  },
  "compute.instances.stop": {
    host: "compute.googleapis.com",
    methods: ["POST"],
    paths: ["/compute/v1/projects/*/zones/*/instances/*/stop"],
  },
  "logging.entries.list": {
    host: "logging.googleapis.com",
    methods: ["POST"],
    paths: [
      "/v2/entries:list",
      "/google.logging.v2.LoggingServiceV2/ListLogEntries",
    ],
  },
  "trace.traces.list": {
    host: "cloudtrace.googleapis.com",
    methods: ["GET"],
    paths: ["/v1/projects/*/traces*"],
  },
  "container.clusters.get": {
    host: "container.googleapis.com",
    methods: ["GET"],
    paths: ["/v1/projects/*/locations/*/clusters/*"],
  },
  "iap.tunnel": {
    host: "tunnel.cloudproxy.app",
    methods: ["GET", "POST"],
    paths: ["/v4/connect*"],
  },
};

export function appendGooglePolicy(
  policy: IntegrationPolicyJson,
  resolved: readonly ResolvedIntegrationGrant[],
): void {
  for (const { grant, connection } of resolved) {
    if (connection.provider !== "gcp") continue;
    if (!connection.enabled) {
      throw new ConnectError(
        `Google Cloud connection "${connection.alias}" is disabled`,
        Code.FailedPrecondition,
      );
    }
    const config = assertGoogleCloudConfig(connection.config);
    const curated = CURATED_GOOGLE_OPERATIONS[grant.operation];
    const hosts = curated ? [curated.host] : config.endpoints;
    if (!curated && grant.operation !== "api.call" && grant.operation !== "gke.api.call") {
      throw new ConnectError(
        `unknown Google Cloud operation "${grant.operation}"`,
        Code.InvalidArgument,
      );
    }
    if (curated && !config.endpoints.includes(curated.host)) {
      throw new ConnectError(
        `Google Cloud connection "${connection.alias}" does not enable ${curated.host}`,
        Code.FailedPrecondition,
      );
    }
    for (const host of hosts) {
      const isGoogleApi = host.endsWith(".googleapis.com");
      if (grant.operation === "api.call" && !isGoogleApi) continue;
      if (grant.operation === "gke.api.call" && isGoogleApi) continue;
      const entry: IntegrationInjectJson = {
        hosts: [host],
        header_name: "",
        header_template: "",
        secret_ref: "",
        mint_source: {
          connection: {
            connection_id: connection.id,
            provider: connection.provider,
          },
        },
        methods: curated?.methods ?? [],
        path_globs: constrainedPaths(
          grant.operation,
          curated?.paths ?? [],
          grant.resourceConstraints,
        ).map((path) => `segment:${path}`),
        graphql_operation: "",
        graphql_field: "",
      };
      const conflict = policy.injects.find((candidate) =>
        candidate.hosts.includes(host) &&
        candidate.mint_source != null &&
        candidate.mint_source.connection.connection_id !== connection.id &&
        matchersOverlap(candidate, entry)
      );
      if (conflict) {
        throw new ConnectError(
          `Google Cloud grants select conflicting credentials for ${host}`,
          Code.InvalidArgument,
        );
      }
      policy.injects.push(entry);
      if (!policy.network.allow_hosts.includes(host)) policy.network.allow_hosts.push(host);
    }
  }
}
