import { useQuery } from "@connectrpc/connect-query";
import { listHosts } from "../gen/engram/app/v1/fleet-FleetService_connectquery";
import type { HostView, HostStatus } from "../lib/types";
import type { HostView as ProtoHostView } from "../gen/engram/app/v1/fleet_pb";

function protoHostToLegacy(h: ProtoHostView): HostView {
  return {
    id: h.id,
    hostname: h.hostname,
    status: h.status as HostStatus,
    capacity_total_mib: Number(h.capacityTotalMib),
    capacity_used_mib: Number(h.capacityUsedMib),
    running_sandboxes: h.runningSandboxes,
    util_disk_total_mib: Number(h.utilDiskTotalMib),
    util_disk_used_mib: Number(h.utilDiskUsedMib),
    util_mem_total_mib: Number(h.utilMemTotalMib),
    util_mem_used_mib: Number(h.utilMemUsedMib),
    util_cpu_pct: h.utilCpuPct,
    util_base_shm_mib: Number(h.utilBaseShmMib),
    util_parked_pss_mib: Number(h.utilParkedPssMib),
    util_running_pss_mib: Number(h.utilRunningPssMib),
    last_heartbeat_at: h.lastHeartbeatAt,
    failing_capabilities: h.failingCapabilities,
    fc_snapshot_version: h.fcSnapshotVersion,
    capabilities_schema: h.capabilitiesSchema,
    live_materializes: h.liveMaterializes,
    live_capture_jobs: h.liveCaptureJobs,
  };
}

/**
 * The operator cockpit polls at 1 s. A passive consumer (the sidebar health
 * telltale, mounted on every page) passes its own relaxed interval; the query
 * key is shared, so the fastest mounted observer sets the real cadence and an
 * idle app never polls the fleet at cockpit speed.
 */
export function useHosts(intervalMs = 1000) {
  return useQuery(
    listHosts,
    {},
    {
      select: (data) => data.hosts.map(protoHostToLegacy),
      refetchInterval: intervalMs,
      refetchOnWindowFocus: false,
      staleTime: 0,
      placeholderData: (prev) => prev,
    },
  );
}
