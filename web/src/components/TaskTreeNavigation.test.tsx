import { create } from "@bufbuild/protobuf";
import { afterEach, describe, expect, test, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";

import { TaskSchema } from "../gen/engram/app/v1/task_pb";
import { TaskTreeNavigation, taskTreeRows } from "./TaskTreeNavigation";

vi.mock("@tanstack/react-router", () => ({
  Link: ({
    children,
    params,
    to: _to,
    ...props
  }: {
    children: React.ReactNode;
    params: { id: string };
    [key: string]: unknown;
  }) => (
    <a href={`/sessions/${params.id}`} {...props}>
      {children}
    </a>
  ),
}));

afterEach(cleanup);

const grandchild = create(TaskSchema, {
  id: "grandchild",
  type: "subsession",
  title: "Generated grandchild title",
  localTaskName: "parser",
  canonicalTaskName: "research/parser",
  parentTaskId: "child",
  rootTaskId: "root",
  status: "done",
  sessions: [{ sessionId: "session-grandchild", role: "primary" }],
});
const child = create(TaskSchema, {
  id: "child",
  type: "subsession",
  title: "Generated child title",
  localTaskName: "research",
  canonicalTaskName: "research",
  parentTaskId: "root",
  rootTaskId: "root",
  status: "working",
  sessions: [{ sessionId: "session-child", role: "primary" }],
});
const root = create(TaskSchema, {
  id: "root",
  type: "chat",
  title: "Design uploads",
  status: "working",
  sessions: [{ sessionId: "session-root", role: "primary" }],
  descendants: [grandchild, child],
});

describe("TaskTreeNavigation", () => {
  test("sorts the flat descendants into root-relative tree order", () => {
    expect(taskTreeRows(root).map(({ task, depth }) => [task.id, depth])).toEqual([
      ["root", 0],
      ["child", 1],
      ["grandchild", 2],
    ]);
  });

  test("uses local child names, links every session, and marks the current task", () => {
    render(<TaskTreeNavigation rootTask={root} currentSessionId="session-child" />);

    expect(screen.getByText("Design uploads")).toBeTruthy();
    expect(screen.getByText("research")).toBeTruthy();
    expect(screen.getByText("parser")).toBeTruthy();
    expect(screen.queryByText("Generated child title")).toBeNull();
    expect(screen.getByRole("tree", { name: "Subtasks" })).toBeTruthy();
    expect(screen.getByText("2 subtasks")).toBeTruthy();
    expect(screen.getByRole("treeitem", { name: /Design uploadsroot/i })).toBeTruthy();

    const current = screen.getByRole("treeitem", { name: /researchworking/i });
    expect(current.getAttribute("aria-current")).toBe("page");
    expect(current.getAttribute("href")).toBe("/sessions/session-child");
    expect(screen.getByRole("treeitem", { name: /parserdone/i }).getAttribute("aria-level")).toBe(
      "3",
    );
  });
});
