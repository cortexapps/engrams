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

function task(id: string, title: string, sessionId: string) {
  return {
    id,
    type: "chat",
    title,
    status: "open",
    createdByUserId: "test-user-id",
    sourceJson: "{}",
    createdAt: AT,
    titleIsCustom: false,
    sessions: [
      {
        sessionId,
        session: {
          id: sessionId,
          status: "active",
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
  const { rows, openId } = useRailSessions();
  return (
    <>
      <div data-testid="rows">{rows.map((r) => r.id).join(",")}</div>
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
