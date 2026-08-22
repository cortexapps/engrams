import { describe, expect, it, vi } from "vitest";
import { fireEvent, screen } from "@testing-library/react";

import { renderWithProviders } from "@/test-utils";
import type { BlockDef } from "@/lib/automation-blocks";

import { BlockList, TRIGGER_ROW_ID } from "./BlockList";

const blocks: BlockDef[] = [
  { id: "launch", type: "create_session", config: { profileId: "p", promptTemplate: "go" } },
  {
    id: "gate",
    type: "branch",
    config: { conditions: { mode: "all", conditions: [] } },
    then: [{ id: "hot", type: "end_session", config: { session: { blockId: "launch" } } }],
    else: [],
  },
  {
    id: "poll",
    type: "loop",
    config: { maxIterations: 3 },
    body: [
      {
        id: "tick",
        type: "run_command",
        config: { session: { blockId: "launch" }, commandTemplate: "true" },
      },
    ],
  },
];

function mount(locked: boolean) {
  const onInsert = vi.fn();
  const onMove = vi.fn();
  const onRemove = vi.fn();
  const onSelect = vi.fn();
  renderWithProviders(
    <BlockList
      trigger={{ kind: "manual" }}
      triggerSummary="Manual"
      blocks={blocks}
      selectedId="launch"
      onSelect={onSelect}
      erroredIds={new Set(["gate"])}
      locked={locked}
      onInsert={onInsert}
      onMove={onMove}
      onRemove={onRemove}
    />,
  );
  return { onInsert, onMove, onRemove, onSelect };
}

describe("BlockList", () => {
  it("renders nested branch/loop children and marks errored rows", async () => {
    mount(false);
    await screen.findByTestId("block-row-launch");
    for (const id of ["launch", "gate", "hot", "poll", "tick"]) {
      expect(screen.getByTestId(`block-row-${id}`)).toBeTruthy();
    }
    expect(screen.getByText("then")).toBeTruthy();
    expect(screen.getByText("else")).toBeTruthy();
    expect(screen.getByText("repeat")).toBeTruthy();
    expect(screen.getByTestId("block-row-launch").getAttribute("aria-current")).toBe("true");
    expect(
      screen.getByTestId("block-row-gate").querySelector('[aria-label="has errors"]'),
    ).not.toBeNull();
    // Nested rows indent.
    expect(screen.getByTestId("block-row-hot").style.marginLeft).toBe("16px");
  });

  it("on a user automation: selects, removes, reorders by drag, and offers insert-between", async () => {
    const h = mount(false);
    await screen.findByTestId("block-row-gate");
    fireEvent.click(screen.getByTestId("block-row-gate"));
    expect(h.onSelect).toHaveBeenCalledWith("gate");
    fireEvent.click(screen.getByTestId("block-row-trigger"));
    expect(h.onSelect).toHaveBeenCalledWith(TRIGGER_ROW_ID);

    fireEvent.click(screen.getByLabelText("Remove hot"));
    expect(h.onRemove).toHaveBeenCalledWith("hot");

    // Drag launch (index 0) onto poll (index 2) within the root list.
    const from = screen.getByTestId("block-row-launch");
    const to = screen.getByTestId("block-row-poll");
    fireEvent.dragStart(from, { dataTransfer: { effectAllowed: "" } });
    fireEvent.dragOver(to);
    fireEvent.drop(to);
    expect(h.onMove).toHaveBeenCalledWith({ root: true }, 0, 2);

    expect(screen.getAllByLabelText("Insert block here").length).toBeGreaterThan(0);
  });

  it("on a built-in: structure is locked — no insert, remove, or drag, and a lock notice", async () => {
    mount(true);
    await screen.findByTestId("block-row-launch");
    expect(screen.queryByLabelText("Insert block here")).toBeNull();
    expect(screen.queryByLabelText("Remove hot")).toBeNull();
    expect(screen.getByTestId("block-row-launch").getAttribute("draggable")).toBe("false");
    expect(screen.getByText(/structure set by the built-in/)).toBeTruthy();
  });
});
