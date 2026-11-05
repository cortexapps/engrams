import { describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRouterTransport } from "@connectrpc/connect";

import { FleetService } from "../gen/engram/app/v1/fleet_pb";
import { renderWithProviders } from "../test-utils";
import { formatBacklog, Storage } from "./Storage";

describe("formatBacklog", () => {
  it("reads an exact backlog as digits and an estimate as a compact approximation", () => {
    expect(formatBacklog(312, true)).toBe("312");
    expect(formatBacklog(22_703_147, false)).toBe("≈22.7M");
    expect(formatBacklog(100_000, false)).toBe("≈100K");
  });
});

describe("Storage page", () => {
  it("shows the snapshot and GC totals and runs GC as a dry run", async () => {
    const chunkGc = vi.fn((_req: { dryRun?: boolean }) => ({
      listedChunks: 1000n,
      malformedKeys: 0n,
      pinSetSize: 900n,
      candidatesMarked: 312n,
      promotedDeletes: 0n,
      promoteDeleteErrors: 0n,
      graceSecs: 3600n,
      generationMoved: false,
    }));
    const transport = createRouterTransport((router) => {
      router.service(FleetService, {
        getStorageSummary: () => ({
          snapshots: 1284n,
          snapshotBytes: 2n * 1024n * 1024n * 1024n * 1024n,
          gcPending: 22_703_147n,
          gcPendingExact: false,
        }),
        chunkGc,
      });
    });
    renderWithProviders(<Storage />, { transport });

    expect(await screen.findByText("1284")).toBeTruthy();
    expect(screen.getByText("≈22.7M")).toBeTruthy();
    expect(screen.getByText("candidates · estimate")).toBeTruthy();

    await userEvent.click(screen.getByRole("button", { name: "GC dry run" }));
    await waitFor(() => expect(chunkGc).toHaveBeenCalled());
    expect(chunkGc).toHaveBeenCalledWith(
      expect.objectContaining({ dryRun: true }),
      expect.anything(),
    );
  });
});
