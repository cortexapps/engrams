import { act, fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, test } from "vitest";

import { SpecTicketTree } from "./SpecTicketTree";
import type { SpecTicket, SpecTicketCommand, SpecTicketTree as Tree } from "@/hooks/useSpecTickets";

const SECTIONS = [
  { id: "sec-data", title: "Data model" },
  { id: "sec-api", title: "API" },
  { id: "sec-failure", title: "Failure modes" },
];

function ticket(overrides: Partial<SpecTicket> & { id: string; title: string }): SpecTicket {
  const sectionId = overrides.backlink?.sectionId ?? "sec-data";
  return {
    parentId: null,
    ordinal: 0,
    depth: 0,
    body: `body of ${overrides.id}`,
    description: `[§Data model](/specs/spec-1?view=spec&section=${sectionId})\n\nbody of ${overrides.id}`,
    backlink: {
      sectionId,
      sectionTitle: SECTIONS.find((section) => section.id === sectionId)?.title ?? sectionId,
      href: `/specs/spec-1?view=spec&section=${sectionId}`,
    },
    dependsOn: [],
    syncState: "draft",
    linearId: null,
    syncError: null,
    openQuestions: [],
    ...overrides,
  };
}

/** The mock 2l tree: four roots, one already nested. */
function tree(): Tree {
  return {
    specId: "spec-1",
    checkpointId: "checkpoint-1",
    docSeq: "18",
    publishedAt: "2026-08-12T15:04:00.000Z",
    sections: SECTIONS,
    tickets: [
      ticket({ id: "columns", title: "Add org quota columns", ordinal: 0 }),
      ticket({ id: "rollup", title: "Hourly meter rollup job", ordinal: 1 }),
      ticket({
        id: "events",
        title: "Emit sandbox.created meter events",
        parentId: "rollup",
        ordinal: 0,
        depth: 1,
        backlink: { sectionId: "sec-api", sectionTitle: "API", href: "/api" },
      }),
      ticket({ id: "limiter", title: "Enforce org quota in the gateway limiter", ordinal: 2 }),
      ticket({
        id: "payload",
        title: "Quota-aware 429 payload",
        ordinal: 3,
        backlink: { sectionId: "sec-api", sectionTitle: "API", href: "/api" },
        openQuestions: [{ id: "q6", sectionId: "sec-api", text: "Does it carry the reset time?" }],
      }),
    ],
    unattachedQuestions: [],
  };
}

/**
 * The editor drives its own tree, exactly as the page does. The command is
 * recorded and the optimistic tree is echoed back, so the test reads what the
 * person sees rather than what a mocked server would have said.
 */
function Harness({
  initial,
  onCommand,
  onTree,
}: {
  initial: Tree;
  onCommand: (command: SpecTicketCommand) => Promise<Tree>;
  onTree?: (next: Tree) => void;
}) {
  const [current, setCurrent] = useState(initial);
  return (
    <SpecTicketTree
      tree={current}
      onCommand={onCommand}
      onTree={(next) => {
        onTree?.(next);
        setCurrent(next);
      }}
    />
  );
}

/**
 * `trees` records every tree the editor showed, newest last. The first entry
 * after a gesture is the optimistic one — the tree the person sees before the
 * reply lands, which is the thing worth asserting.
 */
function renderTree(onCommand?: (command: SpecTicketCommand) => Promise<Tree>) {
  const commands: SpecTicketCommand[] = [];
  const trees: Tree[] = [];
  let latest = tree();
  const record = (command: SpecTicketCommand) => {
    commands.push(command);
    return onCommand ? onCommand(command) : Promise.resolve(latest);
  };
  render(
    <Harness
      initial={latest}
      onCommand={record}
      onTree={(next) => {
        latest = next;
        trees.push(next);
      }}
    />,
  );
  return { commands, trees };
}

/** Every row, in render order, with the shape a person can see. */
function rows(): Array<{ title: string; depth: string }> {
  return [...screen.getByRole("list", { name: "Ticket tree" }).querySelectorAll("li")].map(
    (row) => ({
      title: within(row as HTMLElement).getByRole("button", { expanded: false }).textContent ?? "",
      depth: (row as HTMLElement).dataset["depth"] ?? "",
    }),
  );
}

function grip(title: string): HTMLElement {
  return screen.getByRole("button", { name: `Reorder ${title}` });
}

function row(title: string): HTMLElement {
  const found = [...screen.getByRole("list", { name: "Ticket tree" }).querySelectorAll("li")].find(
    (candidate) => candidate.textContent?.includes(title),
  );
  if (!found) throw new Error(`no row for ${title}`);
  return found as HTMLElement;
}

describe("the ticket tree", () => {
  test("renders the pinned revision, the tree shape and every backlink", () => {
    renderTree();
    expect(screen.getByText("Tickets · 5")).toBeTruthy();
    expect(rows().map((entry) => entry.depth)).toEqual(["0", "0", "1", "0", "0"]);
    const backlinks = screen.getAllByRole("link").map((link) => link.textContent);
    expect(backlinks).toEqual(["§Data model", "§Data model", "§API", "§Data model", "§API"]);
    // The question rides on the ticket covering its section (R29).
    expect(within(row("Quota-aware 429 payload")).getByText("⚑1")).toBeTruthy();
  });

  test("a drag onto another row nests it and closes the gap it left", () => {
    const { commands } = renderTree();

    // One synchronous batch, so no re-render lands between the two events —
    // the drop must still know which row is in the air.
    act(() => {
      fireEvent.dragStart(grip("Enforce org quota in the gateway limiter"));
      fireEvent.dragOver(row("Add org quota columns"));
      fireEvent.drop(row("Add org quota columns"));
    });

    // The dropped row is now a child of the row it landed on …
    expect(rows()).toEqual([
      { title: "Add org quota columns", depth: "0" },
      { title: "Enforce org quota in the gateway limiter", depth: "1" },
      { title: "Hourly meter rollup job", depth: "0" },
      { title: "Emit sandbox.created meter events", depth: "1" },
      { title: "Quota-aware 429 payload", depth: "0" },
    ]);
    // … and the request says exactly that.
    expect(commands).toEqual([{ kind: "move", id: "limiter", parentId: "columns" }]);
  });

  test("a nest leaves the ordinals dense on both sides of the move", () => {
    const { commands, trees } = renderTree();

    fireEvent.dragStart(grip("Quota-aware 429 payload"));
    fireEvent.drop(row("Hourly meter rollup job"));

    const moved = trees[0]!;
    expect(commands).toEqual([{ kind: "move", id: "payload", parentId: "rollup" }]);
    expect(ordinalsOf(moved, null)).toEqual([0, 1, 2]);
    expect(ordinalsOf(moved, "rollup")).toEqual([0, 1]);
    expect(childTitles(moved, "rollup")).toEqual([
      "Emit sandbox.created meter events",
      "Quota-aware 429 payload",
    ]);
  });

  test("a drop onto one's own child is refused, and the tree does not move", () => {
    const { commands } = renderTree();

    fireEvent.dragStart(grip("Hourly meter rollup job"));
    fireEvent.drop(row("Emit sandbox.created meter events"));

    expect(commands).toEqual([]);
    expect(screen.getByRole("alert").textContent).toContain("own subtree");
    expect(rows().map((entry) => entry.title)).toEqual([
      "Add org quota columns",
      "Hourly meter rollup job",
      "Emit sandbox.created meter events",
      "Enforce org quota in the gateway limiter",
      "Quota-aware 429 payload",
    ]);
  });

  test("a merge folds the sources into the first pick and adopts their children", async () => {
    const user = userEvent.setup();
    const { commands, trees } = renderTree();

    await user.click(screen.getByRole("checkbox", { name: "Select Quota-aware 429 payload" }));
    await user.click(screen.getByRole("checkbox", { name: "Select Hourly meter rollup job" }));
    await user.click(screen.getByRole("button", { name: /Merge/ }));

    expect(commands).toEqual([{ kind: "merge", targetId: "payload", sourceIds: ["rollup"] }]);
    const merged = trees[0]!;
    expect(merged.tickets.map((entry) => entry.id)).toEqual([
      "columns",
      "limiter",
      "payload",
      "events",
    ]);
    // The folded ticket's child moved under the survivor, not to the root.
    expect(childTitles(merged, "payload")).toEqual(["Emit sandbox.created meter events"]);
    expect(ordinalsOf(merged, null)).toEqual([0, 1, 2]);
    expect(ordinalsOf(merged, "payload")).toEqual([0]);
    expect(merged.tickets.find((entry) => entry.id === "payload")?.depth).toBe(0);
    expect(merged.tickets.find((entry) => entry.id === "events")?.depth).toBe(1);
  });

  test("a merge into one's own descendant is refused, and the picks survive", async () => {
    const user = userEvent.setup();
    const { commands } = renderTree();

    // The first pick is the target, so picking the child before its parent asks
    // to fold an ancestor into its own descendant.
    await user.click(
      screen.getByRole("checkbox", { name: "Select Emit sandbox.created meter events" }),
    );
    await user.click(screen.getByRole("checkbox", { name: "Select Hourly meter rollup job" }));
    await user.click(screen.getByRole("button", { name: /Merge/ }));

    expect(commands).toEqual([]);
    expect(screen.getByRole("alert").textContent).toContain("own descendants");
    // Nothing moved, and both picks are still picked.
    expect(rows().map((entry) => entry.title)).toEqual([
      "Add org quota columns",
      "Hourly meter rollup job",
      "Emit sandbox.created meter events",
      "Enforce org quota in the gateway limiter",
      "Quota-aware 429 payload",
    ]);
    for (const name of [
      "Select Emit sandbox.created meter events",
      "Select Hourly meter rollup job",
    ]) {
      expect((screen.getByRole("checkbox", { name }) as HTMLInputElement).checked).toBe(true);
    }
  });

  test("the merge button needs two tickets", async () => {
    const user = userEvent.setup();
    renderTree();
    const merge = screen.getByRole("button", { name: /Merge/ });
    expect(merge.hasAttribute("disabled")).toBe(true);
    await user.click(screen.getByRole("checkbox", { name: "Select Add org quota columns" }));
    expect(merge.hasAttribute("disabled")).toBe(true);
    await user.click(screen.getByRole("checkbox", { name: "Select Hourly meter rollup job" }));
    expect(merge.hasAttribute("disabled")).toBe(false);
  });

  test("the keyboard nests, outdents and reorders like the drag does", () => {
    const { commands } = renderTree();

    fireEvent.keyDown(grip("Hourly meter rollup job"), { key: "ArrowRight" });
    expect(commands.at(-1)).toEqual({ kind: "move", id: "rollup", parentId: "columns" });

    fireEvent.keyDown(grip("Emit sandbox.created meter events"), { key: "ArrowLeft" });
    expect(commands.at(-1)).toEqual({ kind: "move", id: "events", parentId: "columns" });

    fireEvent.keyDown(grip("Quota-aware 429 payload"), { key: "ArrowUp" });
    expect(commands.at(-1)).toMatchObject({ kind: "move", id: "payload", parentId: null });
  });

  test("a refused move puts the tree back and says why", async () => {
    const { commands } = renderTree(() => Promise.reject(new Error("A ticket cannot be nested.")));

    fireEvent.dragStart(grip("Enforce org quota in the gateway limiter"));
    fireEvent.drop(row("Add org quota columns"));
    expect(commands).toHaveLength(1);

    expect((await screen.findByRole("alert")).textContent).toContain("A ticket cannot be nested.");
    expect(rows().map((entry) => entry.depth)).toEqual(["0", "0", "1", "0", "0"]);
  });

  test("editing a ticket sends the title, the body and the backlink", async () => {
    const user = userEvent.setup();
    const { commands } = renderTree();

    await user.click(screen.getByRole("button", { name: "Add org quota columns" }));
    await user.clear(screen.getByLabelText("Title"));
    await user.type(screen.getByLabelText("Title"), "Add org quota columns + backfill");
    await user.selectOptions(screen.getByLabelText("Backlink"), "sec-failure");
    await user.click(screen.getByRole("button", { name: "Save ticket" }));

    expect(commands).toEqual([
      {
        kind: "update",
        id: "columns",
        title: "Add org quota columns + backfill",
        body: "body of columns",
        sectionId: "sec-failure",
      },
    ]);
  });

  test("add, split and delete each send one command", async () => {
    const user = userEvent.setup();
    const { commands } = renderTree();

    await user.click(screen.getByRole("button", { name: /Add ticket/ }));
    await user.click(screen.getByRole("button", { name: "Split Quota-aware 429 payload" }));
    await user.click(screen.getByRole("button", { name: "Delete Add org quota columns" }));

    expect(commands.map((command) => command.kind)).toEqual(["add", "split", "delete"]);
    expect(commands[0]).toMatchObject({ parentId: null, sectionId: "sec-data" });
    expect(commands[2]).toEqual({ kind: "delete", id: "columns" });
  });

  test("an open question no ticket covers is reported, not dropped", () => {
    const orphaned = tree();
    orphaned.unattachedQuestions = [
      { id: "q7", sectionId: "sec-failure", text: "How long is the shadow count?" },
    ];
    render(<Harness initial={orphaned} onCommand={() => Promise.resolve(orphaned)} />);
    const block = screen.getByRole("region", { name: "Open questions with no ticket" });
    expect(block.textContent).toContain("How long is the shadow count?");
    expect(block.textContent).toContain("1 open question no ticket covers");
  });

  test("a merge keeps a carried question on the survivor", async () => {
    const user = userEvent.setup();
    const { trees } = renderTree();

    await user.click(screen.getByRole("checkbox", { name: "Select Quota-aware 429 payload" }));
    await user.click(
      screen.getByRole("checkbox", { name: "Select Emit sandbox.created meter events" }),
    );
    await user.click(screen.getByRole("button", { name: /Merge/ }));

    const merged = trees[0]!;
    const survivor = merged.tickets.find((entry) => entry.id === "payload");
    expect(merged.tickets.map((entry) => entry.id)).not.toContain("events");
    expect(survivor?.openQuestions.map((question) => question.id)).toEqual(["q6"]);
    expect(merged.unattachedQuestions).toEqual([]);
  });
});

function childTitles(current: Tree, parentId: string | null): string[] {
  return current.tickets
    .filter((entry) => entry.parentId === parentId)
    .sort((left, right) => left.ordinal - right.ordinal)
    .map((entry) => entry.title);
}

function ordinalsOf(current: Tree, parentId: string | null): number[] {
  return current.tickets
    .filter((entry) => entry.parentId === parentId)
    .map((entry) => entry.ordinal)
    .sort((left, right) => left - right);
}
