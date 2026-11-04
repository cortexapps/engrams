import { describe, expect, test } from "vitest";
import type { HostStatus, HostView } from "./lib/types";
import { deriveHealthMetrics, operatorIssues } from "./operator-health";

const host = (
  status: HostStatus,
  total = 100,
  used = 0,
  failingCapabilities: string[] = [],
): HostView => ({
  id: "h",
  hostname: "h",
  status,
  capacity_total_mib: total,
  capacity_used_mib: used,
  running_sandboxes: 0,
  util_disk_total_mib: 0,
  util_disk_used_mib: 0,
  util_mem_total_mib: 0,
  util_mem_used_mib: 0,
  util_cpu_pct: 0,
  util_base_shm_mib: 0,
  util_parked_pss_mib: 0,
  util_running_pss_mib: 0,
  last_heartbeat_at: "",
  failing_capabilities: failingCapabilities,
  fc_snapshot_version: "",
  capabilities_schema: 0,
  live_materializes: 0,
  live_capture_jobs: 0,
});
describe("deriveHealthMetrics", () => {
  test("empty fleet reads zero capacity", () => {
    const m = deriveHealthMetrics([]);
    expect(m).toEqual({ dead: 0, draining: 0, capPct: 0, capsFailing: 0 });
  });

  test("hosts with a failing capability are tallied", () => {
    const m = deriveHealthMetrics([
      host("ready", 100, 0, ["grpc_self_connect"]),
      host("ready"),
      host("ready", 100, 0, ["session_disk"]),
    ]);
    expect(m.capsFailing).toBe(2);
  });

  test("capacity is summed across hosts and rounded", () => {
    const m = deriveHealthMetrics([host("ready", 1000, 600), host("ready", 1000, 320)]);
    expect(m.capPct).toBe(46); // 920 / 2000
  });

  test("host states are tallied", () => {
    const m = deriveHealthMetrics([host("ready"), host("draining"), host("dead"), host("dead")]);
    expect(m).toMatchObject({ dead: 2, draining: 1 });
  });
});

describe("operatorIssues", () => {
  const base = { dead: 0, draining: 0, capPct: 10, capsFailing: 0 } as const;

  test("a healthy fleet has no issues", () => {
    expect(operatorIssues({ ...base })).toEqual([]);
  });

  test("a host failing capability checks is a caution issue", () => {
    expect(operatorIssues({ ...base, capsFailing: 1 })).toEqual([
      { kind: "caps", tone: "caution", text: "1 host failing capability checks" },
    ]);
  });

  test("a dead host is critical and named with a count", () => {
    expect(operatorIssues({ ...base, dead: 2 })).toEqual([
      { kind: "offline", tone: "critical", text: "2 hosts offline" },
    ]);
  });

  test("capacity crosses caution at 70 and critical at 90, never both", () => {
    expect(operatorIssues({ ...base, capPct: 75 })).toEqual([
      { kind: "capacity", tone: "caution", text: "fleet at 75% capacity" },
    ]);
    expect(operatorIssues({ ...base, capPct: 95 })).toEqual([
      { kind: "capacity", tone: "critical", text: "fleet at 95% capacity" },
    ]);
  });

  test("worst issue sorts first regardless of input order", () => {
    const issues = operatorIssues({ dead: 1, draining: 2, capPct: 95, capsFailing: 1 });
    expect(issues[0].tone).toBe("critical"); // never a caution item at the head
    expect(
      issues.every((i, x) => x === 0 || i.tone !== "critical" || issues[x - 1].tone === "critical"),
    ).toBe(true);
  });
});
