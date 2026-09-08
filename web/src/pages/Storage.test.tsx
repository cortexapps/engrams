import { describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { create } from "@bufbuild/protobuf";
import { createRouterTransport } from "@connectrpc/connect";

import { DurabilityRowSchema, FleetService } from "../gen/engram/app/v1/fleet_pb";
import { TaskService } from "../gen/engram/app/v1/task_pb";
import { renderWithProviders } from "../test-utils";
import type { DurabilityRow } from "../lib/types";
import { flushTone, splitLedger, Storage } from "./Storage";

function row(id: string, over: Partial<DurabilityRow> = {}): DurabilityRow {
  return {
    sandbox_id: `sb-${id}`,
    session_id: `se-${id}`,
    host_id: "host-01",
    dirty_chunks: 0,
    dirty_bytes: 0,
    base_chunks: 100,
    base_chunks_local: 97,
    last_flush_at: new Date().toISOString(),
    ...over,
  };
}

describe("flushTone", () => {
  it("judges staleness against the window, never recency", () => {
    expect(flushTone(new Date(Date.now() - 4_000).toISOString())).toBe("nominal");
    expect(flushTone(new Date(Date.now() - 48_000).toISOString())).toBe("nominal");
    expect(flushTone(new Date(Date.now() - 90_000).toISOString())).toBe("caution");
    expect(flushTone(null)).toBe("muted");
  });
});

describe("splitLedger", () => {
  it("keeps the rows with unflushed work, most at risk first, and folds the rest", () => {
    const { active, quiet } = splitLedger([
      row("a"),
      row("b", { dirty_chunks: 12, dirty_bytes: 9_000 }),
      row("c", { dirty_chunks: 400, dirty_bytes: 118_000 }),
      row("d"),
    ]);
    expect(active.map((r) => r.sandbox_id)).toEqual(["sb-c", "sb-b"]);
    expect(quiet.map((r) => r.sandbox_id)).toEqual(["sb-a", "sb-d"]);
  });
});

describe("Storage page", () => {
  it("shows the rollup, folds quiet rows, and runs GC as a dry run", async () => {
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
          gcPending: 312n,
          trackedSandboxes: 3n,
          dirtyChunks: 412n,
          unflushedBytes: 38n * 1024n * 1024n,
          avgLocalityPct: 94,
          rows: [
            create(DurabilityRowSchema, {
              sandboxId: "sb-hot",
              sessionId: "se-hot",
              hostId: "host-02",
              dirtyChunks: 412,
              dirtyBytes: 38n * 1024n * 1024n,
              baseChunks: 100,
              baseChunksLocal: 97,
              lastFlushAt: new Date(Date.now() - 4_000).toISOString(),
            }),
            create(DurabilityRowSchema, {
              sandboxId: "sb-quiet-1",
              sessionId: "se-quiet-1",
              hostId: "host-02",
              baseChunks: 100,
              baseChunksLocal: 100,
              lastFlushAt: new Date(Date.now() - 4_000).toISOString(),
            }),
            create(DurabilityRowSchema, {
              sandboxId: "sb-quiet-2",
              sessionId: "se-quiet-2",
              hostId: "host-05",
              baseChunks: 100,
              baseChunksLocal: 88,
              lastFlushAt: new Date(Date.now() - 4_000).toISOString(),
            }),
          ],
        }),
        chunkGc,
        listHosts: () => ({ hosts: [] }),
      });
      router.service(TaskService, { listTasks: () => ({ tasks: [] }) });
    });
    renderWithProviders(<Storage />, { transport });

    // The count chip and the locality sub-line both say it.
    expect((await screen.findAllByText("3 tracked")).length).toBeGreaterThan(0);
    expect(screen.getByText("1284")).toBeTruthy();
    expect(screen.getByText("across 1 sandbox")).toBeTruthy();
    // Only the row with unflushed work shows; the two quiet ones fold.
    expect(screen.getByText("se-hot")).toBeTruthy();
    expect(screen.queryByText("se-quiet-1")).toBeNull();
    await userEvent.click(screen.getByRole("button", { name: /2 more · nothing unflushed/ }));
    expect(screen.getByText("se-quiet-1")).toBeTruthy();

    await userEvent.click(screen.getByRole("button", { name: "GC dry run" }));
    await waitFor(() => expect(chunkGc).toHaveBeenCalled());
    expect(chunkGc).toHaveBeenCalledWith(
      expect.objectContaining({ dryRun: true }),
      expect.anything(),
    );
  });
});
