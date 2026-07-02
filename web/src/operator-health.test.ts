import { describe, expect, test } from "vitest";
import type { DurabilityRow, HostStatus, HostView, StorageSummaryResponse } from "./lib/types";
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
  local_snapshots: 0,
  util_disk_total_mib: 0,
  util_disk_used_mib: 0,
  util_mem_total_mib: 0,
  util_mem_used_mib: 0,
  util_cpu_pct: 0,
  last_heartbeat_at: "",
  failing_capabilities: failingCapabilities,
});
const row = (last_flush_at: string | null): DurabilityRow => ({
  sandbox_id: "s",
  session_id: null,
  host_id: "h",
  dirty_chunks: 0,
  dirty_bytes: 0,
  base_chunks: 0,
  base_chunks_local: 0,
  last_flush_at,
});
const storage = (p: Partial<StorageSummaryResponse>): StorageSummaryResponse => ({
  snapshots: 0,
  snapshot_bytes: 0,
  gc_pending: 0,
  tracked_sandboxes: 0,
  dirty_chunks: 0,
  unflushed_bytes: 0,
  avg_locality_pct: 0,
  rows: [],
  ...p,
});

const STALE = "2000-01-01T00:00:00Z"; // comfortably past the 60s flush window
const fresh = () => new Date().toISOString();

describe("deriveHealthMetrics", () => {
  test("empty fleet reads zero capacity and no locality", () => {
    const m = deriveHealthMetrics([], undefined);
    expect(m).toEqual({
      dead: 0,
      draining: 0,
      capPct: 0,
      locality: null,
      rpoStale: 0,
      capsFailing: 0,
    });
  });

  test("hosts with a failing capability are tallied", () => {
    const m = deriveHealthMetrics(
      [host("ready", 100, 0, ["grpc_self_connect"]), host("ready"), host("ready", 100, 0, ["nbd"])],
      undefined,
    );
    expect(m.capsFailing).toBe(2);
  });

  test("capacity is summed across hosts and rounded", () => {
    const m = deriveHealthMetrics([host("ready", 1000, 600), host("ready", 1000, 320)], undefined);
    expect(m.capPct).toBe(46); // 920 / 2000
  });

  test("locality stays null until something is chunk-tracked", () => {
    expect(
      deriveHealthMetrics([host("ready")], storage({ tracked_sandboxes: 0, avg_locality_pct: 90 }))
        .locality,
    ).toBeNull();
    expect(
      deriveHealthMetrics([host("ready")], storage({ tracked_sandboxes: 3, avg_locality_pct: 90 }))
        .locality,
    ).toBe(90);
  });

  test("only rows past the flush window count as RPO-stale", () => {
    const m = deriveHealthMetrics(
      [host("ready")],
      storage({
        tracked_sandboxes: 3,
        rows: [row(STALE), row(fresh()), row(null)],
      }),
    );
    expect(m.rpoStale).toBe(2); // the stale one and the never-flushed null
  });

  test("host states are tallied", () => {
    const m = deriveHealthMetrics(
      [host("ready"), host("draining"), host("dead"), host("dead")],
      undefined,
    );
    expect(m).toMatchObject({ dead: 2, draining: 1 });
  });
});

describe("operatorIssues", () => {
  const base = {
    dead: 0,
    draining: 0,
    capPct: 10,
    locality: null,
    rpoStale: 0,
    capsFailing: 0,
  } as const;

  test("a healthy fleet has no issues", () => {
    expect(operatorIssues({ ...base })).toEqual([]);
  });

  test("a host failing capability checks is a caution issue", () => {
    expect(operatorIssues({ ...base, capsFailing: 1 })).toEqual([
      { tone: "caution", text: "1 host failing capability checks" },
    ]);
  });

  test("a dead host is critical and named with a count", () => {
    expect(operatorIssues({ ...base, dead: 2 })).toEqual([
      { tone: "critical", text: "2 hosts offline" },
    ]);
  });

  test("capacity crosses caution at 70 and critical at 90, never both", () => {
    expect(operatorIssues({ ...base, capPct: 75 })).toEqual([
      { tone: "caution", text: "fleet at 75% capacity" },
    ]);
    expect(operatorIssues({ ...base, capPct: 95 })).toEqual([
      { tone: "critical", text: "fleet at 95% capacity" },
    ]);
  });

  test("low locality is critical, mid locality is caution", () => {
    expect(operatorIssues({ ...base, locality: 40 })[0]).toMatchObject({ tone: "critical" });
    expect(operatorIssues({ ...base, locality: 65 })[0]).toMatchObject({ tone: "caution" });
  });

  test("worst issue sorts first regardless of input order", () => {
    const issues = operatorIssues({
      dead: 1,
      draining: 2,
      capPct: 95,
      locality: 30,
      rpoStale: 4,
      capsFailing: 1,
    });
    expect(issues[0].tone).toBe("critical"); // never a caution item at the head
    expect(
      issues.every((i, x) => x === 0 || i.tone !== "critical" || issues[x - 1].tone === "critical"),
    ).toBe(true);
  });
});
