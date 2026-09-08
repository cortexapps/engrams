import { describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { create } from "@bufbuild/protobuf";
import { createRouterTransport } from "@connectrpc/connect";

import { FleetService, HostViewSchema } from "./../gen/engram/app/v1/fleet_pb";
import { renderWithProviders } from "../test-utils";
import type { HostView } from "../lib/types";
import { deriveHealthMetrics } from "../operator-health";
import { Fleet, fleetVerdict, hostNames, residentSandboxes } from "./Fleet";

function host(id: string, status: HostView["status"], over: Partial<HostView> = {}): HostView {
  return {
    id,
    hostname: id,
    status,
    capacity_total_mib: 64 * 1024,
    capacity_used_mib: 32 * 1024,
    running_sandboxes: 4,
    util_disk_total_mib: 1024 * 1024,
    util_disk_used_mib: 400 * 1024,
    util_mem_total_mib: 64 * 1024,
    util_mem_used_mib: 37 * 1024,
    util_cpu_pct: 22,
    util_base_shm_mib: 0,
    util_parked_pss_mib: 0,
    util_running_pss_mib: 0,
    last_heartbeat_at: "2026-09-08T12:00:00Z",
    failing_capabilities: [],
    fc_snapshot_version: "1.9.1",
    capabilities_schema: 1,
    live_materializes: 0,
    live_capture_jobs: 0,
    ...over,
  };
}

describe("fleetVerdict", () => {
  it("names the empty fleet as a fact, not a fault", () => {
    const v = fleetVerdict([], deriveHealthMetrics([], undefined));
    expect(v.tone).toBe("muted");
    expect(v.headline).toBe("No hosts registered");
  });

  it("is nominal with a full sentence when nothing is wrong", () => {
    const hosts = [host("host-01", "ready")];
    const v = fleetVerdict(hosts, deriveHealthMetrics(hosts, undefined));
    expect(v.tone).toBe("nominal");
    expect(v.headline).toBe("All nominal");
  });

  it("explains a draining host by name", () => {
    const hosts = [host("host-01", "ready"), host("host-04", "draining")];
    const v = fleetVerdict(hosts, deriveHealthMetrics(hosts, undefined));
    expect(v.tone).toBe("caution");
    expect(v.headline).toBe("Caution · 1 host draining");
    expect(v.sentence).toBe("Sandboxes on host-04 finish in place; nothing new lands there.");
  });

  it("lets a critical issue outrank a caution one", () => {
    const hosts = [host("host-02", "dead"), host("host-04", "draining")];
    const v = fleetVerdict(hosts, deriveHealthMetrics(hosts, undefined));
    expect(v.tone).toBe("critical");
    expect(v.headline).toBe("Critical · 1 host offline");
    expect(v.sentence).toContain("host-02 stopped heartbeating");
  });
});

describe("helpers", () => {
  it("sums resident sandboxes from the hosts' own counts", () => {
    expect(
      residentSandboxes([
        host("a", "ready", { running_sandboxes: 6 }),
        host("b", "ready", { running_sandboxes: 9 }),
      ]),
    ).toBe(15);
  });

  it("names up to two hosts, then counts", () => {
    expect(hostNames(["h1"])).toBe("h1");
    expect(hostNames(["h1", "h2"])).toBe("h1 and h2");
    expect(hostNames(["h1", "h2", "h3", "h4"])).toBe("h1, h2 and 2 more");
  });
});

describe("Fleet page", () => {
  it("reads every figure from host truth and offers drain / undrain by status", async () => {
    const uncordon = vi.fn(() => ({ hostId: "host-04", status: "ready" }));
    const transport = createRouterTransport((router) => {
      router.service(FleetService, {
        listHosts: () => ({
          hosts: [
            create(HostViewSchema, {
              id: "host-01",
              hostname: "host-01",
              status: "ready",
              capacityTotalMib: 65536n,
              capacityUsedMib: 32768n,
              runningSandboxes: 6,
              utilMemTotalMib: 65536n,
              utilMemUsedMib: 37888n,
              utilDiskTotalMib: 1048576n,
              utilDiskUsedMib: 409600n,
              utilCpuPct: 22,
              fcSnapshotVersion: "1.9.1",
              capabilitiesSchema: 1,
            }),
            create(HostViewSchema, {
              id: "host-04",
              hostname: "host-04",
              status: "draining",
              capacityTotalMib: 65536n,
              runningSandboxes: 3,
              capabilitiesSchema: 1,
            }),
          ],
        }),
        getStorageSummary: () => ({
          snapshots: 0n,
          snapshotBytes: 0n,
          gcPending: 0n,
          trackedSandboxes: 0n,
          dirtyChunks: 0n,
          unflushedBytes: 0n,
          avgLocalityPct: 0,
          rows: [],
        }),
        uncordonHost: uncordon,
      });
    });
    renderWithProviders(<Fleet />, { transport });

    expect(await screen.findByText("2 hosts")).toBeTruthy();
    expect(screen.getByText("Caution · 1 host draining")).toBeTruthy();
    // Resident sandboxes come from the heartbeats (6 + 3), not the task list.
    expect(screen.getByText("9")).toBeTruthy();
    expect(screen.getByText("host-01")).toBeTruthy();
    expect(screen.getByText("fc 1.9.1")).toBeTruthy();
    // No lime badge for a host state: a dot and the word.
    expect(screen.getByText("draining").closest("[data-slot='badge']")).toBeNull();

    expect(screen.getByRole("button", { name: "Drain" })).toBeTruthy();
    await userEvent.click(screen.getByRole("button", { name: "Undrain" }));
    await waitFor(() => expect(uncordon).toHaveBeenCalled());
  });
});
