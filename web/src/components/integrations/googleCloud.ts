export const GOOGLE_CLOUD_PROVIDER = "gcp";

export interface GoogleCloudOperation {
  action: string;
  label: string;
  access: "read" | "write";
  /**
   * The exact Google API host the curated operation calls — mirrors the
   * orchestrator's `CURATED_GOOGLE_OPERATIONS` table. `null` marks the two
   * endpoint-driven operations (`api.call`, `gke.api.call`) whose hosts come
   * from the connection's own endpoint list.
   */
  host: string | null;
}

export const GOOGLE_CLOUD_OPERATIONS: readonly GoogleCloudOperation[] = [
  {
    action: "compute.instances.get",
    label: "Describe Compute Engine instances",
    access: "read",
    host: "compute.googleapis.com",
  },
  {
    action: "compute.instances.start",
    label: "Start Compute Engine instances",
    access: "write",
    host: "compute.googleapis.com",
  },
  {
    action: "compute.instances.stop",
    label: "Stop Compute Engine instances",
    access: "write",
    host: "compute.googleapis.com",
  },
  {
    action: "logging.entries.list",
    label: "Read Cloud Logging entries",
    access: "read",
    host: "logging.googleapis.com",
  },
  {
    action: "trace.traces.list",
    label: "List Cloud Trace traces",
    access: "read",
    host: "cloudtrace.googleapis.com",
  },
  {
    action: "trace.traces.get",
    label: "Read Cloud Trace details",
    access: "read",
    host: "cloudtrace.googleapis.com",
  },
  {
    action: "monitoring.metricdescriptors.list",
    label: "List Cloud Monitoring metric descriptors",
    access: "read",
    host: "monitoring.googleapis.com",
  },
  {
    action: "monitoring.timeseries.list",
    label: "Read Cloud Monitoring time series",
    access: "read",
    host: "monitoring.googleapis.com",
  },
  {
    action: "container.clusters.get",
    label: "Get GKE cluster credentials",
    access: "read",
    host: "container.googleapis.com",
  },
  {
    action: "iap.tunnel",
    label: "Open IAP tunnels",
    access: "write",
    host: "tunnel.cloudproxy.app",
  },
  {
    action: "gke.api.call",
    label: "Call the configured GKE API server",
    access: "write",
    host: null,
  },
  { action: "api.call", label: "Call configured Google APIs", access: "write", host: null },
];

const isGoogleApiHost = (host: string) => host.endsWith(".googleapis.com");

/**
 * The operations a connection can actually exercise, given its allowed
 * endpoints. Mirrors the orchestrator's `appendGooglePolicy` gate: a curated
 * operation needs its exact host in the endpoint list; `api.call` needs at
 * least one `*.googleapis.com` endpoint; `gke.api.call` needs at least one
 * endpoint that is NOT a Google API host (a GKE control-plane address).
 */
export function googleOperationsForEndpoints(endpoints: readonly string[]): GoogleCloudOperation[] {
  return GOOGLE_CLOUD_OPERATIONS.filter((operation) => {
    if (operation.action === "api.call") return endpoints.some(isGoogleApiHost);
    if (operation.action === "gke.api.call")
      return endpoints.some((host) => !isGoogleApiHost(host));
    return operation.host !== null && endpoints.includes(operation.host);
  });
}

export function googleOperationLabel(action: string): string {
  return GOOGLE_CLOUD_OPERATIONS.find((operation) => operation.action === action)?.label ?? action;
}
