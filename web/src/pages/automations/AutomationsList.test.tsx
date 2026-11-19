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
import { TagSchema, TagSummarySchema } from "@/gen/engram/app/v1/tag_pb";
import { renderWithProviders } from "../../test-utils";
import { AutomationsList, orderAutomations } from "./AutomationsList";

const setEnabled = vi.hoisted(() => vi.fn().mockResolvedValue({}));
const duplicate = vi.hoisted(() => vi.fn().mockResolvedValue({ automation: { id: "copy-1" } }));
const state = vi.hoisted(() => ({
  automations: [] as unknown[],
  builtin: null as unknown,
  pending: false,
  tagSummaries: [] as unknown[],
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
vi.mock("@/hooks/useTags", () => ({
  useTags: () => ({ data: { tags: state.tagSummaries }, isPending: false, error: null }),
}));
vi.mock("@/hooks/useIntegrations", () => ({
  useConnectors: () => ({ data: { connectors: [] } }),
}));

function tagOf(name: string) {
  return create(TagSchema, { id: `t-${name}`, name, color: "#1f6feb", description: "" });
}

function tagSummary(name: string, automationCount = 1) {
  return create(TagSummarySchema, { tag: tagOf(name), automationCount });
}

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
    scoped?: boolean;
    tags?: string[];
    description?: string;
  } = {},
): AutomationSummary {
  return create(AutomationSummarySchema, {
    automation: create(AutomationSchema, {
      id,
      name,
      kind: opts.kind ?? "user",
      enabled: opts.enabled ?? true,
      currentVersion: 1,
      tags: (opts.tags ?? []).map(tagOf),
      description: opts.description ?? "",
      ...(opts.scoped
        ? {
            version: create(AutomationVersionSchema, {
              automationId: id,
              number: 1,
              definitionJson: JSON.stringify({
                engine: 1,
                trigger: { kind: "manual" },
                blocks: [],
                inputsSchema: [],
                agent: {
                  scope: {
                    kind: "key",
                    keyTemplate:
                      "${{ event.raw.repository.full_name }}#${{ event.raw.pull_request.number }}",
                  },
                  profileId: "p",
                  promptTemplate: "go",
                },
                settings: { endSessionsOnFinish: false },
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
  state.tagSummaries = [];
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
      scoped: true,
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
      "Tags",
      "Trigger",
      "Last run",
      "7 days",
      "On",
    ]);
    const rows = await screen.findAllByTestId("automation-row");
    expect(rows).toHaveLength(3);
    expect(rows[0]!.getAttribute("data-kind")).toBe("builtin");
    expect(within(rows[0]!).getByText("built-in")).toBeTruthy();
    expect(within(rows[0]!).getByText("per PR")).toBeTruthy();
    expect(screen.queryByRole("button", { name: /archive/i })).toBeNull();
    // Duplicate lives on a built-in's own editor, not on the list.
    expect(screen.queryByRole("button", { name: /duplicate/i })).toBeNull();
  });

  it("hides the scope badge in a narrow table so the name stays readable", async () => {
    renderWithProviders(<AutomationsList />);
    expect((await screen.findByRole("table")).classList.contains("@container")).toBe(true);
    const rows = await screen.findAllByTestId("automation-row");
    expect(within(rows[0]!).getByText("per PR").classList.contains("@max-[68rem]:hidden")).toBe(
      true,
    );
    expect(within(rows[0]!).getByText("built-in").classList.contains("@max-[68rem]:hidden")).toBe(
      false,
    );
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

  it("shows the empty state with New automation as the one way to start", async () => {
    state.automations = [];
    renderWithProviders(<AutomationsList />);
    expect(await screen.findByText(/no automations yet/i)).toBeTruthy();
    expect(screen.queryByRole("button", { name: /duplicate/i })).toBeNull();
    // Header action + the empty-state call to action.
    expect(screen.getAllByRole("link", { name: /new automation/i })).toHaveLength(2);
  });
});

describe("AutomationsList tag filter", () => {
  const SPECIAL = "a&b/50%";

  beforeEach(() => {
    state.automations = [
      summary("a-one", "One", { tags: ["Ops"] }),
      summary("a-two", "Two", { tags: [SPECIAL] }),
      summary("a-three", "Three"),
    ];
    state.tagSummaries = [tagSummary("Ops"), tagSummary(SPECIAL)];
  });

  const rowNames = async () =>
    (await screen.findAllByTestId("automation-row")).map(
      (row) => within(row).getAllByRole("link")[0]!.textContent,
    );

  it("shows each automation's tags in its row", async () => {
    renderWithProviders(<AutomationsList />, { initialPath: "/automations" });
    const rows = await screen.findAllByTestId("automation-row");
    expect(within(rows[0]!).getByText("Ops")).toBeTruthy();
    expect(within(rows[1]!).queryByText("Ops")).toBeNull();
  });

  it("puts the tags in their own column and keeps the description in the name link", async () => {
    state.automations = [
      summary("a-one", "One", { tags: ["Ops"], description: "Runs every night" }),
    ];
    renderWithProviders(<AutomationsList />, { initialPath: "/automations" });
    const [row] = await screen.findAllByTestId("automation-row");
    const [nameCell, tagsCell] = within(row!).getAllByRole("cell");
    expect(nameCell!.querySelector(".tag-chip")).toBeNull();
    expect(within(nameCell!).getByRole("link").textContent).toContain("Runs every night");
    expect([...tagsCell!.querySelectorAll(".tag-chip")].map((chip) => chip.textContent)).toEqual([
      "Ops",
    ]);
  });

  it("filters by a tag from the URL and shows the count as shown of total", async () => {
    renderWithProviders(<AutomationsList />, { initialPath: "/automations?tags=Ops" });
    expect(await rowNames()).toEqual(["One"]);
    expect(screen.getByText("1 of 3")).toBeTruthy();
    expect(screen.getByRole("button", { name: /^Tag: Ops/ })).toBeTruthy();
  });

  it("filters untagged automations with No tags", async () => {
    renderWithProviders(<AutomationsList />, {
      initialPath: `/automations?tags=${encodeURIComponent(JSON.stringify([""]))}`,
    });
    expect(await rowNames()).toEqual(["Three"]);
  });

  it("round-trips a tag name with URL-special characters", async () => {
    const user = userEvent.setup();
    const first = renderWithProviders(<AutomationsList />, {
      initialPath: "/automations?tags=Ops",
    });
    await user.click(await screen.findByRole("button", { name: /^Tag: Ops/ }));
    await user.click(await screen.findByRole("option", { name: SPECIAL }));
    await waitFor(() =>
      expect(first.router.state.location.search).toEqual({ tags: ["Ops", SPECIAL] }),
    );
    expect(await rowNames()).toEqual(["One", "Two"]);

    const href = first.router.state.location.href;
    first.unmount();
    renderWithProviders(<AutomationsList />, { initialPath: href });
    expect(await rowNames()).toEqual(["One", "Two"]);
  });

  it("keeps a stale tag from the URL selectable and shows the empty result", async () => {
    const user = userEvent.setup();
    renderWithProviders(<AutomationsList />, { initialPath: "/automations?tags=Gone" });
    expect(await screen.findByText("No automations have these tags.")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Clear filter" })).toBeTruthy();
    // The stale name is an option, so it can be unselected like any tag.
    await user.click(screen.getByRole("button", { name: /^Tag: Gone/ }));
    await user.click(await screen.findByRole("option", { name: "Gone" }));
    // The list sorts by name.
    expect(await rowNames()).toEqual(["One", "Three", "Two"]);
  });

  it("shows each tag's color in the filter menu, and no swatch for No tags or a stale name", async () => {
    const user = userEvent.setup();
    state.tagSummaries = [
      tagSummary("Ops"),
      create(TagSummarySchema, {
        tag: create(TagSchema, {
          id: "t-billing",
          name: "Billing",
          color: "#8250df",
          description: "",
        }),
        automationCount: 0,
      }),
    ];
    renderWithProviders(<AutomationsList />, {
      initialPath: `/automations?tags=${encodeURIComponent(JSON.stringify(["Ops", "Gone"]))}`,
    });
    await user.click(await screen.findByRole("button", { name: /^Tag: Ops/ }));
    const swatch = (name: string) =>
      (
        screen.getByRole("option", { name }).querySelector(".tag-chip") as HTMLElement | null
      )?.style.getPropertyValue("--tag-color");
    await screen.findByRole("option", { name: "Billing" });
    expect(swatch("Billing")).toBe("#8250df");
    expect(swatch("Ops")).toBe("#1f6feb");
    expect(swatch("Gone")).toBeUndefined();
    expect(swatch("No tags")).toBeUndefined();
  });

  it("links Manage tags to Settings", async () => {
    renderWithProviders(<AutomationsList />, { initialPath: "/automations" });
    const link = await screen.findByRole("link", { name: "Manage tags" });
    expect(link.getAttribute("href")).toBe("/settings/tags");
  });

  it("hides the filter when no tag exists", async () => {
    state.tagSummaries = [];
    renderWithProviders(<AutomationsList />, { initialPath: "/automations" });
    await screen.findAllByTestId("automation-row");
    expect(screen.queryByRole("button", { name: "Filter" })).toBeNull();
  });
});
