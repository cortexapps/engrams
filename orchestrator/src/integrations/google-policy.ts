/** Curated Google API request policy compilation (ADR 0109). */

import { ConnectError, Code } from "@connectrpc/connect";

import type { IntegrationInjectJson, IntegrationPolicyJson } from "../connectors/registry.ts";
import { assertGoogleCloudConfig } from "./google-wif.ts";
import type { ResolvedIntegrationGrant } from "./grants.ts";

/**
 * One curated operation's egress surface. The proxy matches `methods` and
 * `path_globs` independently INSIDE one inject entry, so the REST paths and
 * the gRPC full-method paths must never share an entry: a shared
 * `["GET","POST"]` entry would allow `POST` on a REST resource path, which is
 * the provider's WRITE verb (for example `timeSeries.create`).
 */
export interface GoogleOperationPolicy {
  host: string;
  /** Operator-facing name. Served through the integration catalog, so the web
   * does not keep its own copy of this table. */
  label: string;
  /** Whether granting this lets a session CHANGE something. Derived from the
   * REST verbs below and asserted against them by the catalog-sync test. */
  access: "read" | "write";
  /** REST surface: these methods bind to these path globs only. Resource
   * constraints narrow these paths. */
  rest: { methods: string[]; paths: string[] };
  /** gRPC full-method paths. Always POST-only, in their own inject entry.
   * A resource constraint cannot scope a gRPC request body, so a constrained
   * grant drops the gRPC surface. */
  grpcPaths: string[];
  /** Resource-constraint rule:
   *  - a RegExp: a constraint must be an API path this expression accepts;
   *  - "none": the operation takes no resource constraints, because the HTTP
   *    path boundary cannot enforce them. */
  constraint: RegExp | "none";
}

/**
 * ONE module-scope table per operation: host, REST/gRPC surfaces, and the
 * constraint validator. Keeping these in one record (instead of two parallel
 * tables keyed by operation name) makes surface/validator drift impossible,
 * and the RegExp values compile once at module load.
 * Exported for the catalog-sync test only.
 */
export const CURATED_GOOGLE_OPERATIONS: Record<string, GoogleOperationPolicy> = {
  "compute.instances.get": {
    host: "compute.googleapis.com",
    label: "Describe Compute Engine instances",
    access: "read",
    rest: { methods: ["GET"], paths: ["/compute/v1/projects/*/zones/*/instances/*"] },
    grpcPaths: [],
    constraint: /^\/compute\/v1\/projects\/[^/]+\/zones\/[^/]+\/instances\/[^/]+$/,
  },
  "compute.instances.start": {
    host: "compute.googleapis.com",
    label: "Start Compute Engine instances",
    access: "write",
    rest: { methods: ["POST"], paths: ["/compute/v1/projects/*/zones/*/instances/*/start"] },
    grpcPaths: [],
    constraint: /^\/compute\/v1\/projects\/[^/]+\/zones\/[^/]+\/instances\/[^/]+\/start$/,
  },
  "compute.instances.stop": {
    host: "compute.googleapis.com",
    label: "Stop Compute Engine instances",
    access: "write",
    rest: { methods: ["POST"], paths: ["/compute/v1/projects/*/zones/*/instances/*/stop"] },
    grpcPaths: [],
    constraint: /^\/compute\/v1\/projects\/[^/]+\/zones\/[^/]+\/instances\/[^/]+\/stop$/,
  },
  "logging.entries.list": {
    host: "logging.googleapis.com",
    label: "Read Cloud Logging entries",
    access: "read",
    // The Logging REST list endpoint is POST by API design.
    rest: { methods: ["POST"], paths: ["/v2/entries:list"] },
    grpcPaths: ["/google.logging.v2.LoggingServiceV2/ListLogEntries"],
    // The resource filter rides in the request body, not the path.
    constraint: "none",
  },
  "trace.traces.list": {
    host: "cloudtrace.googleapis.com",
    label: "List Cloud Trace traces",
    access: "read",
    rest: { methods: ["GET"], paths: ["/v1/projects/*/traces"] },
    grpcPaths: [],
    constraint: /^\/v1\/projects\/[^/]+\/traces$/,
  },
  "trace.traces.get": {
    host: "cloudtrace.googleapis.com",
    label: "Read Cloud Trace details",
    access: "read",
    rest: { methods: ["GET"], paths: ["/v1/projects/*/traces/*"] },
    grpcPaths: [],
    constraint: /^\/v1\/projects\/[^/]+\/traces\/[^/]+$/,
  },
  "monitoring.metricdescriptors.list": {
    host: "monitoring.googleapis.com",
    label: "List Cloud Monitoring metric descriptors",
    access: "read",
    rest: { methods: ["GET"], paths: ["/v3/projects/*/metricDescriptors"] },
    grpcPaths: ["/google.monitoring.v3.MetricService/ListMetricDescriptors"],
    constraint: /^\/v3\/projects\/[^/]+\/metricDescriptors$/,
  },
  "monitoring.timeseries.list": {
    host: "monitoring.googleapis.com",
    label: "Read Cloud Monitoring time series",
    access: "read",
    rest: { methods: ["GET"], paths: ["/v3/projects/*/timeSeries"] },
    grpcPaths: ["/google.monitoring.v3.MetricService/ListTimeSeries"],
    constraint: /^\/v3\/projects\/[^/]+\/timeSeries$/,
  },
  "container.clusters.get": {
    host: "container.googleapis.com",
    label: "Get GKE cluster credentials",
    access: "read",
    rest: { methods: ["GET"], paths: ["/v1/projects/*/locations/*/clusters/*"] },
    grpcPaths: [],
    constraint: /^\/v1\/projects\/[^/]+\/locations\/[^/]+\/clusters\/[^/]+$/,
  },
  "iap.tunnel": {
    host: "tunnel.cloudproxy.app",
    label: "Open IAP tunnels",
    access: "write",
    // The IAP tunnel endpoint upgrades a GET and accepts POST control frames.
    // Both verbs address the same non-REST endpoint, so one entry is correct.
    rest: { methods: ["GET", "POST"], paths: ["/v4/connect*"] },
    grpcPaths: [],
    constraint: /^\/v4\/connect$/,
  },
};

/** The broad pass-through operations that take free-form path constraints. */
/**
 * Credential-producing Google operations, refused at GRANT time.
 *
 * The egress proxy refuses the same surfaces again at request time from its own
 * checked-in table (`crates/engram-egress-proxy/policy/`). Two independent
 * enforcement points on purpose: this one keeps an unusable grant out of the
 * database, and that one holds even if a grant somehow reaches a session.
 */
export const FORBIDDEN_GOOGLE_OPERATIONS: ReadonlySet<string> = new Set([
  "iam.serviceaccountkeys.create",
  "iam.generateaccesstoken",
  "iam.generateidtoken",
  "iam.signblob",
  "iam.signjwt",
]);

export const GOOGLE_PASSTHROUGH_OPERATIONS = ["api.call", "gke.api.call"] as const;

/**
 * The two endpoint-driven operations. They have no fixed host — their reach is
 * whatever the connection's endpoint list allows — so the catalog describes
 * which KIND of endpoint each one needs, and the editor offers it only when
 * the connection has one.
 */
export const GOOGLE_PASSTHROUGH_CATALOG: Record<
  (typeof GOOGLE_PASSTHROUGH_OPERATIONS)[number],
  { label: string; access: "read" | "write"; endpoint: "google-api" | "non-google-api" }
> = {
  "api.call": {
    label: "Call configured Google APIs",
    access: "write",
    endpoint: "google-api",
  },
  "gke.api.call": {
    label: "Call the configured GKE API server",
    access: "write",
    endpoint: "non-google-api",
  },
};

function isPassthroughOperation(operation: string): boolean {
  return (GOOGLE_PASSTHROUGH_OPERATIONS as readonly string[]).includes(operation);
}

function constrainedPaths(operation: string, defaults: string[], constraints: readonly string[]): string[] {
  if (constraints.length === 0) return defaults;
  const curated = CURATED_GOOGLE_OPERATIONS[operation];
  if (curated?.constraint === "none") {
    throw new ConnectError(
      `${operation} resource constraints cannot be enforced at the HTTP path boundary`,
      Code.InvalidArgument,
    );
  }
  const validator = curated?.constraint;
  for (const constraint of constraints) {
    if (
      constraint.length > 2048 ||
      !constraint.startsWith("/") ||
      constraint.includes("://") ||
      constraint.includes("?") ||
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

/**
 * Can two single-segment `*` globs match a common string? `*` stands for any
 * run of non-`/` characters (the proxy's `segment-path:` semantics).
 * Memoized product walk over both patterns.
 */
function segmentGlobsIntersect(a: string, b: string): boolean {
  const memo = new Map<number, boolean>();
  function walk(i: number, j: number): boolean {
    const key = i * (b.length + 1) + j;
    const cached = memo.get(key);
    if (cached !== undefined) return cached;
    let result: boolean;
    if (i === a.length && j === b.length) {
      result = true;
    } else if (i === a.length) {
      result = b.slice(j).split("").every((ch) => ch === "*");
    } else if (j === b.length) {
      result = a.slice(i).split("").every((ch) => ch === "*");
    } else if (a[i] === "*") {
      // The star matches empty, or it produces the character `b` needs next.
      result = walk(i + 1, j) || walk(i, j + 1);
    } else if (b[j] === "*") {
      result = walk(i, j + 1) || walk(i + 1, j);
    } else {
      result = a[i] === b[j] && walk(i + 1, j + 1);
    }
    memo.set(key, result);
    return result;
  }
  return walk(0, 0);
}

/**
 * Can two compiled path globs match a common request path? Google policy
 * emits `segment-path:` globs, where `*` never crosses `/`; two such globs
 * intersect only segment-by-segment. Any other pattern form is treated as
 * overlapping (fail closed).
 */
function pathGlobsIntersect(a: string, b: string): boolean {
  const pa = a.startsWith("segment-path:") ? a.slice("segment-path:".length) : null;
  const pb = b.startsWith("segment-path:") ? b.slice("segment-path:".length) : null;
  if (pa == null || pb == null) return true;
  const segmentsA = pa.split("/");
  const segmentsB = pb.split("/");
  if (segmentsA.length !== segmentsB.length) return false;
  return segmentsA.every((segment, index) => segmentGlobsIntersect(segment, segmentsB[index]!));
}

// STRUCTURAL overlap between two inject matchers: true when some request
// (method, path) satisfies both. Exact string equality is not enough — a
// glob (`/v3/projects/*/timeSeries`) and a constrained exact path
// (`/v3/projects/prod/timeSeries`) both match the same request, and the
// proxy would then credential it with whichever entry it finds first.
function matchersOverlap(a: IntegrationInjectJson, b: IntegrationInjectJson): boolean {
  const methodsOverlap = a.methods.length === 0 || b.methods.length === 0 ||
    a.methods.some((method) => b.methods.includes(method));
  if (!methodsOverlap) return false;
  // Empty means every path.
  if (a.path_globs.length === 0 || b.path_globs.length === 0) return true;
  return a.path_globs.some((pathA) => b.path_globs.some((pathB) => pathGlobsIntersect(pathA, pathB)));
}

/**
 * Validate the SHAPE of Google grants: known operation + valid resource
 * constraints. Pure — no connection state is consulted. Profile save calls
 * this, so a connection that an admin later disables (or whose endpoints an
 * admin edits) never blocks unrelated edits of a granting profile. Connection
 * STATE (enabled, endpoint membership, config validity) is enforced at
 * session-create by `appendGooglePolicy`.
 *
 * Reached through the provider registry, which passes only the grants whose
 * connection is Google — hence no provider test in the loop.
 */
export function validateGoogleGrants(resolved: readonly ResolvedIntegrationGrant[]): void {
  for (const { grant } of resolved) {
    const curated = CURATED_GOOGLE_OPERATIONS[grant.operation];
    if (!curated && !isPassthroughOperation(grant.operation)) {
      throw new ConnectError(
        `unknown Google Cloud operation "${grant.operation}"`,
        Code.InvalidArgument,
      );
    }
    // Validation only; the returned paths are recomputed at compile time.
    constrainedPaths(grant.operation, curated?.rest.paths ?? [], grant.resourceConstraints);
  }
}

/**
 * Compile Google grants into mint-source inject entries at session-create.
 *
 * The hosts a connection opens are NOT added to `network.allow_hosts`:
 * reachability rides the inject entry itself (the proxy intercepts a host
 * with a matching inject). When the boot-time credential mint fails, the
 * coordinator drops the inject — and because nothing else allows the host,
 * the session fails CLOSED instead of leaving an unfiltered bypass path to
 * the provider (S6).
 */
export function appendGooglePolicy(
  policy: IntegrationPolicyJson,
  resolved: readonly ResolvedIntegrationGrant[],
): void {
  validateGoogleGrants(resolved);
  for (const { grant, connection } of resolved) {
    if (!connection.enabled) {
      throw new ConnectError(
        `Google Cloud connection "${connection.alias}" is disabled`,
        Code.FailedPrecondition,
      );
    }
    const config = assertGoogleCloudConfig(connection.config);
    const curated = CURATED_GOOGLE_OPERATIONS[grant.operation];
    const hosts = curated ? [curated.host] : config.endpoints;
    if (curated && !config.endpoints.includes(curated.host)) {
      throw new ConnectError(
        `Google Cloud connection "${connection.alias}" does not enable ${curated.host}`,
        Code.FailedPrecondition,
      );
    }
    // One inject entry per {methods, paths} surface. The proxy matches methods
    // and paths independently inside one entry, so a curated read operation
    // must keep its GET-only REST paths and its POST-only gRPC paths apart.
    const surfaces: Array<{ methods: string[]; paths: string[] }> = curated
      ? [
          {
            methods: curated.rest.methods,
            paths: constrainedPaths(grant.operation, curated.rest.paths, grant.resourceConstraints),
          },
          ...(curated.grpcPaths.length > 0 && grant.resourceConstraints.length === 0
            ? [{ methods: ["POST"], paths: curated.grpcPaths }]
            : []),
        ]
      : [{
          methods: [],
          paths: constrainedPaths(grant.operation, [], grant.resourceConstraints),
        }];
    for (const host of hosts) {
      const isGoogleApi = host.endsWith(".googleapis.com");
      if (grant.operation === "api.call" && !isGoogleApi) continue;
      if (grant.operation === "gke.api.call" && isGoogleApi) continue;
      for (const surface of surfaces) {
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
          methods: surface.methods,
          path_globs: surface.paths.map((path) => `segment-path:${path}`),
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
      }
    }
  }
}
