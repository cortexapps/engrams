import { describe, expect, it } from "vitest";
import { screen } from "@testing-library/react";
import { createRouterTransport } from "@connectrpc/connect";

import { AutomationRunService, AutomationService } from "@/gen/engram/app/v1/automation_pb";
import { renderWithProviders } from "@/test-utils";

import { AgentPage } from "./AgentPage";

const NOW = "2026-08-25T12:00:00Z";
const INSTANCE = {
  id: "instance-1",
  automationId: "auto-1",
  key: "project-ENG_42",
  status: "open",
  inputsJson: '{"project":"ENG-42"}',
  openedBy: "user-1",
  openedAt: NOW,
};

function transport() {
  return createRouterTransport((router) => {
    router.service(AutomationService, {
      listAutomations: () => ({
        automations: [
          {
            automation: {
              id: "auto-1",
              name: "Pull request review",
              version: {
                definitionJson: JSON.stringify({
                  engine: 1,
                  trigger: { kind: "integration", eventKeys: ["pull_request.opened"] },
                  blocks: [],
                  entrypoints: [
                    {
                      id: "comments",
                      trigger: { kind: "webhook", events: ["comment"] },
                      blocks: [],
                    },
                  ],
                  inputsSchema: [],
                  settings: { endSessionsOnFinish: false },
                }),
              },
            },
            triggerSummary: "GitHub",
            runs7d: [],
          },
        ],
      }),
    });
    router.service(AutomationRunService, {
      getInstance: () => ({
        instance: INSTANCE,
        handles: [{ handle: "github:engrams/engrams#42", writtenBy: "run-1", createdAt: NOW }],
      }),
      listInstances: () => ({ instances: [INSTANCE] }),
      listRuns: () => ({
        runs: [
          {
            id: "run-1",
            automationId: "auto-1",
            status: "completed",
            triggerSource: "integration",
            createdAt: NOW,
            instanceId: "instance-1",
          },
        ],
        filtered: [],
      }),
      getRun: () => ({
        run: {
          brief: {
            id: "run-1",
            automationId: "auto-1",
            status: "completed",
            createdAt: NOW,
            instanceId: "instance-1",
          },
          steps: [],
          sessionIds: ["session-42"],
        },
      }),
      closeInstance: () => ({ closed: true }),
    });
  });
}

describe("AgentPage", () => {
  it("titles the agent by the place it owns, with its timeline and session", async () => {
    renderWithProviders(<AgentPage />, {
      transport: transport(),
      initialPath: "/automations/agents/instance-1",
    });

    expect(await screen.findByRole("heading", { name: "engrams/engrams#42" })).toBeTruthy();
    expect(screen.getByText("project-ENG_42")).toBeTruthy();
    expect(screen.getByText("Timeline")).toBeTruthy();
    expect(screen.queryByText("Routing map")).toBeNull();
    expect(await screen.findByRole("link", { name: "Open session ↗" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Close" })).toBeTruthy();
  });
});
