// Contract test for the session delete control: the button is gated behind a
// confirmation dialog, confirming calls DeleteTask with the owning task id
// (which cascades to DeleteSession server-side), and cancelling calls nothing.

import { afterEach, describe, expect, test } from "vitest";
import { cleanup, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRouterTransport } from "@connectrpc/connect";
import { renderWithProviders } from "../../test-utils";
import { DeleteSessionButton } from "./DeleteSessionButton";
import { TaskService } from "../../gen/engram/app/v1/task_pb";

function installTransport() {
  const deleted: string[] = [];
  const transport = createRouterTransport((router) => {
    router.service(TaskService, {
      createTask: () => ({ task: undefined }),
      listTasks: () => ({ tasks: [] }),
      getTask: () => ({ task: undefined }),
      deleteTask: (req) => {
        deleted.push(req.taskId);
        return {};
      },
      updateTask: () => ({ task: undefined }),
    });
  });
  return { transport, deleted };
}

describe("DeleteSessionButton", () => {
  afterEach(cleanup);

  test("confirming deletes the owning task", async () => {
    const { transport, deleted } = installTransport();
    renderWithProviders(<DeleteSessionButton taskId="t1" title="Fix the flaky test" />, {
      transport,
    });
    const user = userEvent.setup();

    // Delete is guarded: the first click only opens the dialog, nothing deleted yet.
    // findBy* waits for TanStack Router's async first mount.
    await user.click(await screen.findByRole("button", { name: "Delete" }));
    expect(await screen.findByText(/Delete "Fix the flaky test"\?/)).toBeTruthy();
    expect(deleted).toHaveLength(0);

    // Confirming in the dialog fires DeleteTask with the owning task id.
    await user.click(screen.getByRole("button", { name: "Delete session" }));
    await waitFor(() => expect(deleted).toEqual(["t1"]));
  });

  test("cancelling deletes nothing", async () => {
    const { transport, deleted } = installTransport();
    renderWithProviders(<DeleteSessionButton taskId="t1" title={null} />, { transport });
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: "Delete" }));
    // No custom title → the dialog falls back to "this session".
    expect(await screen.findByText(/Delete this session\?/)).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Cancel" }));

    await waitFor(() => expect(screen.queryByText(/Delete this session\?/)).toBeNull());
    expect(deleted).toHaveLength(0);
  });
});
