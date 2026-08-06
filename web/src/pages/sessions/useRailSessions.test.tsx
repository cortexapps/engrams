// Contract test for the sessions-rail row set: the rows are exactly the
// server's "My tasks" window, and opening a session never adds one. The rail
// used to prepend the open session, which dropped another user's task (opened
// from the admin fleet list) at the top of a list it does not belong to.

import { describe, expect, test } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import { createRouterTransport } from "@connectrpc/connect";
import { renderWithProviders } from "../../test-utils";
import { TaskService } from "../../gen/engram/app/v1/task_pb";
import { useRailSessions } from "./useRailSessions";

const AT = "2026-08-04T12:00:00Z";

function task(
  id: string,
  title: string,
  sessionId: string,
  opts: { sessionStatus?: string; taskStatus?: string } = {},
) {
  return {
    id,
    type: "chat",
    title,
    status: opts.taskStatus ?? "open",
    createdByUserId: "test-user-id",
    sourceJson: "{}",
    createdAt: AT,
    titleIsCustom: false,
    sessions: [
      {
        sessionId,
        session: {
          id: sessionId,
          status: opts.sessionStatus ?? "active",
          image: "img",
          mode: "agent",
          createdAt: AT,
          lastActiveAt: AT,
        },
      },
    ],
  };
}

/** The whole window the server returns for `scope: "mine"`. "s-foreign"
 * (another user's, from the fleet list) and "s-old" (mine, but ranked past the
 * window) are both absent from it. */
const WINDOW = [task("t-a", "First", "s-a"), task("t-b", "Second", "s-b")];

/** Fails the test if the rail reaches for anything beyond its own window —
 * the point of the change is that an open session costs no extra lookup. */
function installTransport() {
  const calls: string[] = [];
  const transport = createRouterTransport((router) => {
    router.service(TaskService, {
      listTasks: (req) => {
        calls.push(req.search);
        return { tasks: WINDOW, totalCount: WINDOW.length };
      },
      getTask: () => {
        throw new Error("the rail must not call GetTask");
      },
    });
  });
  return { transport, calls };
}

/** Probe: renders the hook's rows as plain text so the assertions read the
 * list itself, with no sidebar chrome in the way. */
function RailProbe() {
  const { rows, visibleRows, groups, banded, openId } = useRailSessions();
  return (
    <>
      <div data-testid="rows">{rows.map((r) => r.id).join(",")}</div>
      <div data-testid="visible">{visibleRows.map((r) => r.id).join(",")}</div>
      <div data-testid="groups">
        {groups.map((g) => `${g.label}:${g.items.map((i) => i.id).join("+")}`).join(",")}
      </div>
      <div data-testid="banded">{String(banded)}</div>
      <div data-testid="statuses">{rows.map((r) => r.status).join(",")}</div>
      <div data-testid="open">{openId ?? ""}</div>
    </>
  );
}

const rowIds = () => screen.getByTestId("rows").textContent?.split(",").filter(Boolean) ?? [];

async function renderRail(path: string) {
  const { transport, calls } = installTransport();
  renderWithProviders(<RailProbe />, { transport, initialPath: path });
  await waitFor(() => expect(rowIds()).toEqual(["s-a", "s-b"]));
  return calls;
}

describe("useRailSessions", () => {
  test("another user's open session is not added to the rail", async () => {
    const calls = await renderRail("/sessions/s-foreign");
    // The route resolves to an open id — the rail just declines to invent a
    // row for it, and asks the server for nothing beyond its own window.
    expect(screen.getByTestId("open").textContent).toBe("s-foreign");
    expect(rowIds()).toEqual(["s-a", "s-b"]);
    expect(calls).toEqual([""]);
  });

  test("my own open session outside the window is not pinned either", async () => {
    await renderRail("/sessions/s-old");
    expect(rowIds()).toEqual(["s-a", "s-b"]);
  });

  test("an open session inside the window keeps its server-ordered place", async () => {
    await renderRail("/sessions/s-b");
    // Second in the window, so second in the rail — not hoisted to the top.
    expect(rowIds()).toEqual(["s-a", "s-b"]);
    expect(screen.getByTestId("open").textContent).toBe("s-b");
  });

  test("the section pages are not mistaken for an open session", async () => {
    await renderRail("/sessions/all");
    expect(screen.getByTestId("open").textContent).toBe("");
  });
});

// The rail bands tasks by what a reader asks of them, not by the eleven-state
// lifecycle. The row order MUST equal the banded order: the ⌥-jump keymap
// navigates to rows[n-1] while the rail draws n on the nth row it renders.
describe("useRailSessions banding", () => {
  const BANDED = [
    // Server order is recency, and every band here is deliberately out of it.
    task("t-1", "Finished", "s-done", { sessionStatus: "completed" }),
    task("t-2", "Snapshotted", "s-idle", { sessionStatus: "idle" }),
    task("t-3", "Running", "s-active", { sessionStatus: "active" }),
    // Awaiting review on an already-snapshotted sandbox: attention outranks
    // the lifecycle, so this leads the rail rather than sitting under "Idle".
    task("t-4", "Asking me something", "s-ask", {
      sessionStatus: "parked",
      taskStatus: "awaiting_review",
    }),
    task("t-5", "Booting", "s-created", { sessionStatus: "created" }),
  ];

  function renderBanded() {
    const transport = createRouterTransport((router) => {
      router.service(TaskService, {
        listTasks: () => ({ tasks: BANDED, totalCount: BANDED.length }),
      });
    });
    renderWithProviders(<RailProbe />, { transport, initialPath: "/sessions" });
  }

  test("bands by what the reader must do, attention first", async () => {
    renderBanded();
    await waitFor(() =>
      expect(screen.getByTestId("groups").textContent).toBe(
        [
          "Needs you:s-ask",
          // Recency order survives inside a band: s-active came before
          // s-created in the server's window.
          "Working:s-active+s-created",
          "Idle:s-idle",
          "Finished:s-done",
        ].join(","),
      ),
    );
  });

  test("the jumpable rows are in the order the rail draws them", async () => {
    renderBanded();
    await waitFor(() =>
      expect(rowIds()).toEqual(["s-ask", "s-active", "s-created", "s-idle", "s-done"]),
    );
  });

  // ⌥1–9 draws its number on the nth row the rail RENDERS, so a band nobody can
  // see must not consume a number. Finished starts collapsed.
  test("a collapsed band is loaded but not jumpable", async () => {
    renderBanded();
    await waitFor(() => expect(rowIds()).toHaveLength(5));
    const visible = screen.getByTestId("visible").textContent?.split(",").filter(Boolean);
    expect(visible).toEqual(["s-ask", "s-active", "s-created", "s-idle"]);
    // Still loaded — the command menu searches what you have, not what is open.
    expect(rowIds()).toContain("s-done");
  });

  test("a healthy response bands, and says so", async () => {
    renderBanded();
    await waitFor(() => expect(screen.getByTestId("banded").textContent).toBe("true"));
  });
});

// The control plane is unreachable, so the orchestrator sends every task with
// its `session` unset and `sessionStateUnavailable` true. Banding that would be
// a story about an outage — and with Finished collapsed by default, the rail
// would look EMPTY on an account full of live work.
describe("useRailSessions with no live session state", () => {
  /** Same tasks, but the orchestrator could not resolve any of their sessions. */
  const STRIPPED = [
    task("t-1", "First", "s-a"),
    task("t-2", "Second", "s-b"),
    task("t-3", "Third", "s-c"),
  ].map((t) => ({ ...t, sessions: [{ sessionId: t.sessions[0]!.sessionId }] }));

  function renderOutage(unavailable: boolean) {
    const transport = createRouterTransport((router) => {
      router.service(TaskService, {
        listTasks: () => ({
          tasks: STRIPPED,
          totalCount: STRIPPED.length,
          sessionStateUnavailable: unavailable,
        }),
      });
    });
    renderWithProviders(<RailProbe />, { transport, initialPath: "/sessions" });
  }

  test("drops to one unlabelled group holding every row", async () => {
    renderOutage(true);
    await waitFor(() => expect(screen.getByTestId("banded").textContent).toBe("false"));
    // One group, no heading — and every row in it, in the order the server sent.
    expect(screen.getByTestId("groups").textContent).toBe(":s-a+s-b+s-c");
    expect(rowIds()).toEqual(["s-a", "s-b", "s-c"]);
  });

  test("every row is jumpable, because nothing is collapsed", async () => {
    renderOutage(true);
    await waitFor(() => expect(rowIds()).toHaveLength(3));
    expect(screen.getByTestId("visible").textContent).toBe("s-a,s-b,s-c");
  });

  test("calls the rows unknown, never dead", async () => {
    renderOutage(true);
    await waitFor(() => expect(rowIds()).toHaveLength(3));
    expect(screen.getByTestId("statuses").textContent).toBe("unknown,unknown,unknown");
  });

  // The field is stated in the NEGATIVE so proto3's false default means "fine".
  // An old server that has never heard of it must not put the rail in its
  // degraded shape — here the same stripped rows, with the flag unset.
  test("an absent flag means the state is fine, and those sessions are gone", async () => {
    renderOutage(false);
    // Wait on the ROWS, not on `banded` — banded is already true before the
    // first fetch lands, so it would pass against an empty rail.
    await waitFor(() => expect(rowIds()).toHaveLength(3));
    expect(screen.getByTestId("banded").textContent).toBe("true");
    expect(screen.getByTestId("statuses").textContent).toBe("dead,dead,dead");
    expect(screen.getByTestId("groups").textContent).toBe("Finished:s-a+s-b+s-c");
  });
});
