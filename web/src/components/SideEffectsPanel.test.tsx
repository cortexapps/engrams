import { afterEach, describe, expect, test } from "vitest";
import { cleanup, screen, waitFor } from "@testing-library/react";
import { createRouterTransport } from "@connectrpc/connect";

import { PrRefService } from "../gen/engram/app/v1/pr_ref_pb";
import { renderWithProviders } from "../test-utils";
import { SideEffectsPanel } from "./SideEffectsPanel";

afterEach(cleanup);

function installTransport() {
  const requests: Array<{ taskId?: string; sessionId?: string }> = [];
  const transport = createRouterTransport((router) => {
    router.service(PrRefService, {
      listPrRefs: (request) => {
        requests.push({
          taskId: request.taskId,
          sessionId: request.sessionId,
        });
        return {
          prRefs: [
            {
              id: "pr-ref-1",
              repo: "openai/engrams",
              prNumber: 97,
              authoringTaskId: request.taskId,
              sessionId: "session-1",
              title: "Record authored pull requests",
              url: "https://github.com/openai/engrams/pull/97",
              headBranch: "adr-0097-prref",
              baseBranch: "adr-0097-writefiles",
            },
          ],
        };
      },
    });
  });
  return { requests, transport };
}

describe("SideEffectsPanel", () => {
  test("queries by task and renders the durable PR link", async () => {
    const { requests, transport } = installTransport();
    renderWithProviders(<SideEffectsPanel taskId="task-1" sessionId="session-1" />, { transport });

    await waitFor(() => expect(screen.getByText("Record authored pull requests")).toBeTruthy());
    expect(requests).toEqual([{ taskId: "task-1", sessionId: undefined }]);
    expect(screen.getByText("openai/engrams#97")).toBeTruthy();
    expect(screen.getByText("adr-0097-prref")).toBeTruthy();
    expect(screen.getByText("adr-0097-writefiles")).toBeTruthy();
    expect(
      screen.getByRole("link", { name: /Record authored pull requests/ }).getAttribute("href"),
    ).toBe("https://github.com/openai/engrams/pull/97");
  });

  test("queries by session when the session has no owning task", async () => {
    const { requests, transport } = installTransport();
    renderWithProviders(<SideEffectsPanel taskId={null} sessionId="orphan-session" />, {
      transport,
    });

    await waitFor(() => expect(requests).toHaveLength(1));
    expect(requests).toEqual([{ taskId: undefined, sessionId: "orphan-session" }]);
  });

  test("shows the quiet empty state", async () => {
    const transport = createRouterTransport((router) => {
      router.service(PrRefService, {
        listPrRefs: () => ({ prRefs: [] }),
      });
    });
    renderWithProviders(<SideEffectsPanel taskId="task-1" sessionId="session-1" />, { transport });

    await waitFor(() => expect(screen.getByText("No side effects recorded")).toBeTruthy());
  });
});
