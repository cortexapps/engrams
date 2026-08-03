// Contract test for the automation editor's harness/model/effort override
// (ADR 0063 B2 applied to ADR 0102 automations): a stored override hydrates the
// shared pickers and round-trips on save, and an automation that inherits the
// profile default sends no override fields at all.

import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { AutomationEditor } from "./AutomationEditor";

interface CreateTaskValue {
  profileId: string;
  promptTemplate: string;
  includeEventContext: boolean;
  harness?: string;
  model?: string;
  effort?: string;
}

const automationHolder = vi.hoisted(() => ({
  value: undefined as undefined | { automation: unknown },
}));
const paramsHolder = vi.hoisted(() => ({ value: {} as { id?: string } }));
const create = vi.hoisted(() => vi.fn().mockResolvedValue({ automation: { id: "new" } }));
const update = vi.hoisted(() => vi.fn().mockResolvedValue({ automation: { id: "a1" } }));
const testRender = vi.hoisted(() => vi.fn().mockResolvedValue({ errors: [] }));

vi.mock("@/hooks/useAutomations", () => ({
  useAutomation: () => ({ data: automationHolder.value, isPending: false, error: null }),
  useAutomationRuns: () => ({ data: { runs: [] }, isPending: false, error: null }),
  useCreateAutomation: () => ({ mutateAsync: create, isPending: false }),
  useUpdateAutomation: () => ({ mutateAsync: update, isPending: false }),
  useTestAutomationRender: () => ({ mutateAsync: testRender, isPending: false }),
  useWebhookEvents: () => ({ data: { events: [], variables: [] }, isPending: false }),
  useWebhookRegistrations: () => ({ data: { registrations: [] } }),
  useWebhookSamples: () => ({ data: { samples: [] } }),
}));
vi.mock("@/hooks/useProfiles", () => ({
  useProfiles: () => ({
    data: { profiles: [{ id: "pf1", name: "Triage agent", harness: "claude" }] },
  }),
}));
// Two harnesses so the harness picker renders (it hides when there is nothing
// to choose), each with its own model enum.
vi.mock("@/hooks/useHarnessCatalog", () => ({
  useHarnessCatalog: () => ({
    data: [
      {
        name: "claude",
        descriptor: {
          label: "Claude Code",
          models: [
            { id: "opus", label: "Claude Opus 4.8", default: true, env: {} },
            { id: "sonnet", label: "Claude Sonnet 4.6", default: false, env: {} },
          ],
          effort: [{ id: "high", label: "High", default: true, env: {} }],
        },
      },
      {
        name: "codex",
        descriptor: {
          label: "Codex CLI",
          models: [{ id: "gpt", label: "GPT", default: true, env: {} }],
          effort: [],
        },
      },
    ],
  }),
}));
vi.mock("@tanstack/react-router", async (orig) => ({
  ...(await orig()),
  useNavigate: () => vi.fn(),
  useParams: () => paramsHolder.value,
  Link: ({
    children,
    to: _to,
    params: _params,
    ...rest
  }: Record<string, unknown> & { children: React.ReactNode }) => <a {...rest}>{children}</a>,
}));

function automation(override: Partial<CreateTaskValue>) {
  return {
    automation: {
      id: "a1",
      name: "Daily triage",
      description: "",
      enabled: true,
      trigger: {
        trigger: { case: "cron", value: { schedule: "0 9 * * *", timezone: "UTC" } },
      },
      action: {
        action: {
          case: "createTask",
          value: {
            profileId: "pf1",
            promptTemplate: "Triage the queue",
            includeEventContext: true,
            ...override,
          },
        },
      },
    },
  };
}

function savedAction(): CreateTaskValue {
  return update.mock.calls[0]![0].action.action.value as CreateTaskValue;
}

beforeEach(() => {
  automationHolder.value = undefined;
  paramsHolder.value = { id: "a1" };
  create.mockClear();
  update.mockClear();
  testRender.mockClear();
});

describe("AutomationEditor harness selection", () => {
  it("hydrates a stored override and round-trips it on save", async () => {
    automationHolder.value = automation({ harness: "codex", model: "gpt" });
    render(<AutomationEditor mode="edit" />);
    await screen.findByDisplayValue("Daily triage");

    await waitFor(() => {
      expect(screen.getByTestId("session-harness-select").textContent).toContain("Codex CLI");
      expect(screen.getByTestId("session-model-select").textContent).toContain("GPT");
    });
    // Codex declares no effort options, so that picker does not render.
    expect(screen.queryByTestId("session-effort-select")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: /save changes/i }));
    await waitFor(() => expect(update).toHaveBeenCalledOnce());
    expect(savedAction()).toMatchObject({ harness: "codex", model: "gpt" });
    expect(savedAction().effort).toBeUndefined();
  });

  it("inherits the profile default and sends no override fields", async () => {
    automationHolder.value = automation({});
    render(<AutomationEditor mode="edit" />);
    await screen.findByDisplayValue("Daily triage");

    // With no override, each trigger states what the launch will actually use:
    // the profile's harness ("claude") and that descriptor's default options.
    await waitFor(() => {
      expect(screen.getByTestId("session-harness-select").textContent).toContain("Claude Code");
    });
    expect(screen.getByTestId("session-model-select").textContent).toContain("Claude Opus 4.8");
    expect(screen.getByTestId("session-effort-select").textContent).toContain("High");

    fireEvent.click(screen.getByRole("button", { name: /save changes/i }));
    await waitFor(() => expect(update).toHaveBeenCalledOnce());
    const action = savedAction();
    expect(action).toMatchObject({ profileId: "pf1", promptTemplate: "Triage the queue" });
    expect(action.harness).toBeUndefined();
    expect(action.model).toBeUndefined();
    expect(action.effort).toBeUndefined();
  });
});
