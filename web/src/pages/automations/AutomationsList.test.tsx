import { describe, it, expect, vi, beforeEach } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { create } from "@bufbuild/protobuf";

import {
  AutomationRunBriefSchema,
  AutomationSchema,
  AutomationSummarySchema,
  AutomationVersionSchema,
  DayRunCountSchema,
  type AutomationSummary,
} from "@/gen/engram/app/v1/automation_pb";
import { renderWithProviders } from "../../test-utils";
import { AutomationsList, orderAutomations } from "./AutomationsList";

const setEnabled = vi.hoisted(() => vi.fn().mockResolvedValue({}));
const duplicate = vi.hoisted(() => vi.fn().mockResolvedValue({ automation: { id: "copy-1" } }));
const state = vi.hoisted(() => ({
  automations: [] as unknown[],
  builtin: null as unknown,
  pending: false,
}));

vi.mock("@/hooks/useAutomations", () => ({
  useAutomations: () => ({
    data: { automations: state.automations },
    isPending: state.pending,
    error: null,
  }),
  useBuiltinAutomation: () => ({ data: state.builtin ? { automation: state.builtin } : undefined }),
  useSetAutomationEnabled: () => ({ mutateAsync: setEnabled, isPending: false }),
  useDuplicateAutomation: () => ({ mutateAsync: duplicate, isPending: false }),
  // The registrations panel (moved verbatim) is exercised by its own tests.
  useWebhookRegistrations: () => ({ data: { registrations: [] }, isPending: false, error: null }),
  useCreateWebhookRegistration: () => ({ mutateAsync: vi.fn(), isPending: false }),
  useDeleteWebhookRegistration: () => ({ mutateAsync: vi.fn(), isPending: false }),
}));
vi.mock("@/hooks/useIntegrations", () => ({
  useConnectors: () => ({ data: { connectors: [] } }),
}));

function summary(
  id: string,
  name: string,
  opts: {
    kind?: string;
    enabled?: boolean;
    lastStatus?: string;
    startedAt?: string;
    endedAt?: string;
    failedToday?: number;
    workstreams?: boolean;
  } = {},
): AutomationSummary {
  return create(AutomationSummarySchema, {
    automation: create(AutomationSchema, {
      id,
      name,
      kind: opts.kind ?? "user",
      enabled: opts.enabled ?? true,
      currentVersion: 1,
      ...(opts.workstreams
        ? {
            version: create(AutomationVersionSchema, {
              automationId: id,
              number: 1,
              definitionJson: JSON.stringify({
                engine: 1,
                trigger: { kind: "manual" },
                blocks: [],
                inputsSchema: [],
                settings: { endSessionsOnFinish: false, instance: { keyTemplate: "{{pr}}" } },
              }),
            }),
          }
        : {}),
    }),
    triggerSummary: "GitHub · PR opened · 2 repos",
    ...(opts.lastStatus
      ? {
          lastRun: create(AutomationRunBriefSchema, {
            id: `run-${id}`,
            automationId: id,
            status: opts.lastStatus,
            startedAt: opts.startedAt ?? new Date().toISOString(),
            ...(opts.endedAt ? { endedAt: opts.endedAt } : {}),
          }),
        }
      : {}),
    runs7d: Array.from({ length: 7 }, (_, i) =>
      create(DayRunCountSchema, {
        day: `d${i}`,
        completed: i,
        failed: i === 6 ? (opts.failedToday ?? 0) : 0,
        filtered: 0,
      }),
    ),
  });
}

beforeEach(() => {
  setEnabled.mockClear();
  duplicate.mockClear();
  state.pending = false;
  state.builtin = create(AutomationSchema, {
    id: "builtin-pr",
    name: "PR review",
    kind: "builtin",
    builtinKey: "pr_review",
  });
  state.automations = [
    summary("a-zeta", "Zeta nightly", { lastStatus: "completed" }),
    summary("a-builtin", "PR review", {
      kind: "builtin",
      lastStatus: "failed",
      startedAt: "2026-08-21T10:00:00Z",
      endedAt: "2026-08-21T10:02:00Z",
      failedToday: 3,
      workstreams: true,
    }),
    summary("a-alpha", "Alpha triage", { enabled: false }),
  ];
});

describe("orderAutomations", () => {
  it("pins built-ins first, then sorts by name", () => {
    const ordered = orderAutomations(state.automations as AutomationSummary[]);
    expect(ordered.map((s) => s.automation?.name)).toEqual([
      "PR review",
      "Alpha triage",
      "Zeta nightly",
    ]);
  });
});

describe("AutomationsList", () => {
  it("renders one table with the built-in pinned and labelled", async () => {
    renderWithProviders(<AutomationsList />);
    expect(await screen.findByRole("table")).toBeTruthy();
    expect(screen.getAllByRole("columnheader").map((cell) => cell.textContent)).toEqual([
      "Automation",
      "Trigger",
      "Last run",
      "7 days",
      "On",
    ]);
    const rows = await screen.findAllByTestId("automation-row");
    expect(rows).toHaveLength(3);
    expect(rows[0]!.getAttribute("data-kind")).toBe("builtin");
    expect(within(rows[0]!).getByText("built-in")).toBeTruthy();
    expect(within(rows[0]!).getByText("workstreams")).toBeTruthy();
    expect(screen.queryByRole("button", { name: /archive/i })).toBeNull();
    expect(screen.getByRole("button", { name: "Duplicate PR review" })).toBeTruthy();
  });

  it("encodes the last run as an instrument-toned status dot", async () => {
    renderWithProviders(<AutomationsList />);
    const rows = await screen.findAllByTestId("automation-row");
    const tone = (row: HTMLElement) =>
      row.querySelector('[data-slot="status-dot"]')?.getAttribute("data-tone");
    expect(tone(rows[0]!)).toBe("critical"); // PR review: failed
    expect(tone(rows[1]!)).toBe("muted"); // Alpha: never run
    expect(tone(rows[2]!)).toBe("nominal"); // Zeta: completed
    expect(within(rows[1]!).getByText("never run")).toBeTruthy();
    expect(within(rows[0]!).getByText("3 today")).toBeTruthy();
    expect(within(rows[0]!).getByText(/2m 0s/)).toBeTruthy();
    expect(rows[0]!.getAttribute("style")).toContain("instrument-critical");
  });

  it("toggles enabled through the mutation", async () => {
    const user = userEvent.setup();
    renderWithProviders(<AutomationsList />);
    const toggle = await screen.findByRole("switch", { name: /turn on alpha triage/i });
    await user.click(toggle);
    await waitFor(() => expect(setEnabled).toHaveBeenCalledWith({ id: "a-alpha", enabled: true }));
  });

  it("renders a sparkline per row linking to the runs tab", async () => {
    renderWithProviders(<AutomationsList />);
    const links = await screen.findAllByRole("link", { name: /activity for/i });
    expect(links).toHaveLength(3);
    expect(links[0]!.getAttribute("href")).toContain("tab=activity");
    expect(links[0]!.querySelector("svg[role=img]")).not.toBeNull();
  });

  it("shows the empty state with 'Duplicate PR review' only when the built-in exists", async () => {
    state.automations = [];
    state.builtin = null;
    renderWithProviders(<AutomationsList />);
    expect(await screen.findByText(/no automations yet/i)).toBeTruthy();
    expect(screen.queryByRole("button", { name: /duplicate/i })).toBeNull();
    // Header action + the empty-state call to action.
    expect(screen.getAllByRole("link", { name: /new automation/i })).toHaveLength(2);
  });

  it("duplicates the built-in from the masthead and the empty state", async () => {
    const user = userEvent.setup();
    state.automations = [];
    state.builtin = create(AutomationSchema, {
      id: "builtin-pr",
      name: "PR review",
      kind: "builtin",
      builtinKey: "pr_review",
    });
    renderWithProviders(<AutomationsList />);
    const buttons = await screen.findAllByRole("button", { name: "Duplicate PR review" });
    expect(buttons).toHaveLength(2);
    await user.click(buttons[0]!);
    await waitFor(() => expect(duplicate).toHaveBeenCalledWith({ automationId: "builtin-pr" }));
  });
});
