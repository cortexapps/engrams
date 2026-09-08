import { describe, expect, it, vi } from "vitest";
import { fireEvent, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { renderWithProviders } from "@/test-utils";
import type { BlockDef } from "@/lib/automation-blocks";

import { Canvas, TRIGGER_ROW_ID } from "./Canvas";

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

function mount(locked: boolean, extra: Partial<Parameters<typeof Canvas>[0]> = {}) {
  const onInsert = vi.fn();
  const onMove = vi.fn();
  const onRemove = vi.fn();
  const onSelect = vi.fn();
  renderWithProviders(
    <Canvas
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
      {...extra}
    />,
  );
  return { onInsert, onMove, onRemove, onSelect };
}

describe("Canvas", () => {
  it("renders every nested block as a node, labels the branch legs, and marks errors", async () => {
    mount(false);
    await screen.findByTestId("block-row-launch");
    for (const id of ["launch", "gate", "hot", "poll", "tick"]) {
      expect(screen.getByTestId(`block-row-${id}`)).toBeTruthy();
    }
    expect(screen.getByText("then")).toBeTruthy();
    expect(screen.getByText("else")).toBeTruthy();
    expect(screen.getByTestId("block-row-launch").getAttribute("aria-current")).toBe("true");
    expect(
      screen.getByTestId("block-row-gate").querySelector('[aria-label="has errors"]'),
    ).not.toBeNull();
    // Nodes are positioned, not indented: the then-leg child sits below its branch.
    const gateTop = Number.parseFloat(screen.getByTestId("block-row-gate").style.top);
    const hotTop = Number.parseFloat(screen.getByTestId("block-row-hot").style.top);
    expect(hotTop).toBeGreaterThan(gateTop);
  });

  it("selects nodes and the trigger; arrows move focus in document order", async () => {
    const h = mount(false);
    await screen.findByTestId("block-row-gate");
    fireEvent.click(screen.getByTestId("block-row-gate"));
    expect(h.onSelect).toHaveBeenCalledWith("gate");
    fireEvent.click(screen.getByTestId("block-row-trigger"));
    expect(h.onSelect).toHaveBeenCalledWith(TRIGGER_ROW_ID);

    const trigger = screen.getByTestId("block-row-trigger");
    trigger.focus();
    fireEvent.keyDown(trigger, { key: "ArrowDown" });
    expect(document.activeElement).toBe(screen.getByTestId("block-row-launch"));
    fireEvent.keyDown(screen.getByTestId("block-row-launch"), { key: "ArrowUp" });
    expect(document.activeElement).toBe(trigger);
  });

  it("moves and removes through the node menu", async () => {
    const user = userEvent.setup();
    const h = mount(false);
    await screen.findByTestId("block-row-hot");

    await user.click(screen.getByLabelText("Actions for launch"));
    await user.click(await screen.findByText("Move down"));
    expect(h.onMove).toHaveBeenCalledWith({ root: true }, 0, 1);

    await user.click(screen.getByLabelText("Actions for hot"));
    await user.click(await screen.findByLabelText("Remove hot"));
    expect(h.onRemove).toHaveBeenCalledWith("hot");

    // Move up is disabled at a list head.
    await user.click(screen.getByLabelText("Actions for launch"));
    const moveUp = await screen.findByText("Move up");
    expect(moveUp.closest('[role="menuitem"]')?.getAttribute("aria-disabled")).toBe("true");
  });

  it("inserts through an edge's + menu, including into an empty else leg", async () => {
    const user = userEvent.setup();
    const h = mount(false);
    await screen.findByTestId("block-row-launch");
    const inserts = screen.getAllByLabelText("Insert block here");
    // 4 root (0..3) + 2 then (0..1) + 1 empty-else + 2 body (0..1).
    expect(inserts).toHaveLength(9);
    await user.click(inserts[0]!);
    await user.click(await screen.findByRole("menuitem", { name: /run command/i }));
    expect(h.onInsert).toHaveBeenCalledWith({ root: true }, 0, "run_command");
  });

  it("locked: no insert or node actions, and the lock notice shows", async () => {
    mount(true);
    await screen.findByTestId("block-row-launch");
    expect(screen.queryByLabelText("Insert block here")).toBeNull();
    expect(screen.queryByLabelText("Actions for hot")).toBeNull();
    expect(screen.getByText(/structure set by the built-in/)).toBeTruthy();
  });

  it("ghosts render dashed and run status renders a chip (future consumers)", async () => {
    mount(false, {
      ghostIds: new Set(["tick"]),
      nodeStatus: { launch: "ok" },
    });
    const tick = await screen.findByTestId("block-row-tick");
    expect(tick.className).toContain("border-dashed");
    expect(
      screen.getByTestId("block-row-launch").querySelector('[aria-label="step ok"]'),
    ).not.toBeNull();
  });
});
