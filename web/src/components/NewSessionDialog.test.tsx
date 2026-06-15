import { expect, test, vi, beforeEach } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRouterTransport } from "@connectrpc/connect";
import { renderWithProviders } from "../test-utils";
import { NewSessionDialog } from "./NewSessionDialog";
import * as imagesHook from "../hooks/useEnabledImages";
import { TaskService } from "../gen/engram/app/v1/task_pb";

beforeEach(() => {
  vi.restoreAllMocks();
  vi.spyOn(imagesHook, "useEnabledImages").mockReturnValue({
    data: [
      {
        id: "1",
        image_uri: "ghcr.io/x/api:warm",
        manifest_digest: "sha256:abc",
        manifest_name: "api",
        manifest_description: null,
        harness_name: "claude",
        last_refreshed_at: new Date().toISOString(),
        created_at: new Date().toISOString(),
      },
    ],
    isLoading: false,
    error: null,
  } as unknown as ReturnType<typeof imagesHook.useEnabledImages>);
});

test("creates a session and reports the new id", async () => {
  const onCreated = vi.fn();

  // ADR 0051 Task 23: agent mode → CreateTask. Wire a transport stub that
  // returns a task with a primary session ref so onCreated gets "sess-1".
  const taskTransport = createRouterTransport((router) => {
    router.service(TaskService, {
      createTask: () => ({
        task: {
          id: "task-1",
          type: "chat",
          status: "open",
          sourceJson: "{}",
          sessions: [{ sessionId: "sess-1" }],
          createdAt: new Date().toISOString(),
        },
      }),
      listTasks: () => ({ tasks: [] }),
      getTask: () => ({ task: undefined }),
      deleteTask: () => ({}),
    });
  });

  renderWithProviders(<NewSessionDialog onCreated={onCreated} />, { transport: taskTransport });
  // Router defers the initial render to a microtask — await the trigger.
  await userEvent.click(await screen.findByRole("button", { name: /new session/i }));
  await userEvent.click(await screen.findByRole("button", { name: /^start$/i }));
  await waitFor(() => expect(onCreated).toHaveBeenCalledWith("sess-1"));
});
