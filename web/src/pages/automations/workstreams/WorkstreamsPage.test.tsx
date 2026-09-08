import { describe, expect, it } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRouterTransport } from "@connectrpc/connect";

import { AutomationRunService, AutomationService } from "@/gen/engram/app/v1/automation_pb";
import { renderWithProviders } from "@/test-utils";

import { WorkstreamsPage } from "./WorkstreamsPage";

const NOW = "2026-08-25T12:00:00Z";

function transport() {
  return createRouterTransport((router) => {
    router.service(AutomationService, {
      listAutomations: () => ({
        automations: [
          {
            automation: {
              id: "auto-1",
              name: "Pull request review",
              inputsJson: '{"team":"platform"}',
              version: {
                definitionJson: JSON.stringify({
                  engine: 1,
                  trigger: { kind: "integration", eventKeys: ["pull_request.opened"] },
                  blocks: [],
                  inputsSchema: [],
                  settings: {
                    endSessionsOnFinish: false,
                    instance: { keyTemplate: "project-{number}" },
                  },
                }),
              },
            },
            triggerSummary: "GitHub",
            runs7d: [],
          },
        ],
      }),
      runNow: () => ({ runId: "run-1" }),
    });
    router.service(AutomationRunService, {
      listInstances: () => ({
        instances: [
          {
            id: "instance-1",
            automationId: "auto-1",
            key: "project-ENG_42",
            status: "open",
            inputsJson: "{}",
            openedBy: "user-1",
            openedAt: NOW,
          },
        ],
      }),
      getInstance: () => ({
        handles: [{ handle: "github:engrams/engrams#42", writtenBy: "run-1", createdAt: NOW }],
      }),
      listRuns: () => ({
        runs: [
          {
            id: "run-1",
            automationId: "auto-1",
            status: "completed",
            createdAt: NOW,
            instanceId: "instance-1",
          },
        ],
        filtered: [],
      }),
      listRecentDrops: () => ({ drops: [] }),
    });
  });
}

describe("WorkstreamsPage", () => {
  it("renders the cross-automation table and opens the automation-first dialog", async () => {
    renderWithProviders(<WorkstreamsPage />, { transport: transport() });

    expect(await screen.findByRole("heading", { name: "Workstreams" })).toBeTruthy();
    expect(await screen.findByText("project ENG 42")).toBeTruthy();
    expect(screen.getByText("project-ENG_42")).toBeTruthy();
    expect(screen.getByText("Pull request review")).toBeTruthy();
    expect(await screen.findByText("engrams/engrams#42")).toBeTruthy();

    await userEvent.click(screen.getByRole("button", { name: "Open a workstream" }));
    await waitFor(() => expect(screen.getByTestId("open-workstream-form")).toBeTruthy());
    expect(screen.getByRole("combobox", { name: "Automation" })).toBeTruthy();
  });
});
