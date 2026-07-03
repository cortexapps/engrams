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
    local_snapshots: Number(h.localSnapshots),
    util_disk_total_mib: Number(h.utilDiskTotalMib),
    util_disk_used_mib: Number(h.utilDiskUsedMib),
    util_mem_total_mib: Number(h.utilMemTotalMib),
    util_mem_used_mib: Number(h.utilMemUsedMib),
    util_cpu_pct: h.utilCpuPct,
    last_heartbeat_at: h.lastHeartbeatAt,
    failing_capabilities: h.failingCapabilities,
    fc_snapshot_version: h.fcSnapshotVersion,
    capabilities_schema: h.capabilitiesSchema,
  };
}

export function useHosts() {
  return useQuery(
    listHosts,
    {},
    {
      select: (data) => data.hosts.map(protoHostToLegacy),
      refetchInterval: 1000,
      refetchOnWindowFocus: false,
      staleTime: 0,
      placeholderData: (prev) => prev,
    },
  );
}
