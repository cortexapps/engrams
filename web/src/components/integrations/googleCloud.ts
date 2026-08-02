export const GOOGLE_CLOUD_PROVIDER = "gcp";

export const GOOGLE_CLOUD_OPERATIONS = [
  {
    action: "compute.instances.get",
    label: "Describe Compute Engine instances",
    access: "read",
  },
  {
    action: "compute.instances.start",
    label: "Start Compute Engine instances",
    access: "write",
  },
  {
    action: "compute.instances.stop",
    label: "Stop Compute Engine instances",
    access: "write",
  },
  { action: "logging.entries.list", label: "Read Cloud Logging entries", access: "read" },
  { action: "trace.traces.list", label: "List Cloud Trace traces", access: "read" },
  { action: "trace.traces.get", label: "Read Cloud Trace details", access: "read" },
  {
    action: "monitoring.metricdescriptors.list",
    label: "List Cloud Monitoring metric descriptors",
    access: "read",
  },
  {
    action: "monitoring.timeseries.list",
    label: "Read Cloud Monitoring time series",
    access: "read",
  },
  {
    action: "container.clusters.get",
    label: "Get GKE cluster credentials",
    access: "read",
  },
  { action: "iap.tunnel", label: "Open IAP tunnels", access: "write" },
  { action: "gke.api.call", label: "Call the configured GKE API server", access: "write" },
  { action: "api.call", label: "Call configured Google APIs", access: "write" },
] as const;
