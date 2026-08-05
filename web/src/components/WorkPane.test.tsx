import { afterEach, describe, expect, test } from "vitest";
import { cleanup, screen, waitFor } from "@testing-library/react";
import { createRouterTransport } from "@connectrpc/connect";

import { PrRefService } from "../gen/engram/app/v1/pr_ref_pb";
import { renderWithProviders } from "../test-utils";
import { WorkPane } from "./WorkPane";

afterEach(cleanup);

describe("WorkPane side effects", () => {
  test("passes the existing task context into the Side effects tab", async () => {
    const taskIds: Array<string | undefined> = [];
    const transport = createRouterTransport((router) => {
      router.service(PrRefService, {
        listPrRefs: (request) => {
          taskIds.push(request.taskId);
          return { prRefs: [] };
        },
      });
    });

    renderWithProviders(
      <WorkPane
        sessionId="session-1"
        taskId="task-1"
        session={undefined}
        events={[]}
        open
        tab="side-effects"
        onTabChange={() => {}}
        processesTailId={null}
        onProcessesTail={() => {}}
        browserEnabled={false}
        ideEnabled={false}
        onCollapse={() => {}}
      />,
      { transport },
    );

    expect(await screen.findByRole("tab", { name: "Side effects" })).toBeTruthy();
    await waitFor(() => expect(taskIds).toEqual(["task-1"]));
    expect(screen.getByText("No side effects recorded")).toBeTruthy();
  });
});
