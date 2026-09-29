import { describe, it, expect, vi, beforeEach } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "../../test-utils";
import { SlackThreads, channelsOf } from "./SlackThreads";

const setEntry = vi.hoisted(() => vi.fn().mockResolvedValue({}));
const state = vi.hoisted(() => ({
  inputs: { channels: {} as Record<string, string> },
  enabled: false,
  profiles: [] as Array<{ id: string; name: string }>,
  instances: [] as Array<Record<string, unknown>>,
}));

vi.mock("../../hooks/useAutomations", () => ({
  useBuiltinAutomation: () => ({
    data: {
      automation: {
        id: "auto-slack",
        enabled: state.enabled,
        inputsJson: JSON.stringify(state.inputs),
      },
    },
    isPending: false,
  }),
  useSetMapInputEntry: () => ({ mutateAsync: setEntry, mutate: setEntry, isPending: false }),
}));
vi.mock("../../hooks/useInstances", () => ({
  useInstanceList: () => ({ data: { instances: state.instances }, isPending: false }),
  useRecentDrops: () => ({ data: { drops: [] } }),
  useInstance: () => ({ data: undefined }),
}));
vi.mock("../../hooks/useAutomationRuns", () => ({
  useRunList: () => ({ data: { runs: [] } }),
}));
vi.mock("../../hooks/useProfiles", () => ({
  useProfiles: () => ({ data: { profiles: state.profiles } }),
}));
vi.mock("../../hooks/useNow", () => ({ useNow: () => Date.parse("2026-09-29T12:00:00Z") }));

beforeEach(() => {
  setEntry.mockClear();
  state.inputs = { channels: { C0123456789: "prof-a" } };
  state.enabled = true;
  state.profiles = [{ id: "prof-a", name: "Helpdesk" }];
  state.instances = [
    {
      id: "ai_1",
      automationId: "auto-slack",
      key: "T1:C0123456789:1700.1",
      status: "open",
      inputsJson: "{}",
      openedBy: "event:slack:Ev1",
      openedAt: "2026-09-29T11:00:00Z",
    },
  ];
});

describe("channelsOf", () => {
  it("reads the channels map and tolerates junk", () => {
    expect(channelsOf(JSON.stringify({ channels: { C2: "p", C1: "q" } }))).toEqual([
      { channel: "C1", profileId: "q" },
      { channel: "C2", profileId: "p" },
    ]);
    expect(channelsOf("nope")).toEqual([]);
    expect(channelsOf(JSON.stringify({ channels: [] }))).toEqual([]);
  });
});

describe("SlackThreads", () => {
  it("lists enrolled channels with their profile, the threads, and the link to the automation", async () => {
    renderWithProviders(<SlackThreads />);
    expect(await screen.findByText("C0123456789")).toBeTruthy();
    expect(screen.getByText("Helpdesk")).toBeTruthy();
    expect(screen.getByRole("link", { name: /Slack threads automation/ })).toBeTruthy();
    // The key shows twice on a workstream row (humanized name + mono key).
    expect((await screen.findAllByText("T1:C0123456789:1700.1")).length).toBeGreaterThan(0);
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("warns when channels are enrolled but the automation is paused", async () => {
    state.enabled = false;
    renderWithProviders(<SlackThreads />);
    expect((await screen.findByRole("alert")).textContent).toMatch(/paused/);
  });

  it("enrolls a channel as one map entry and enables the automation", async () => {
    renderWithProviders(<SlackThreads />);
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /enroll channel/i }));
    await user.type(await screen.findByLabelText("Channel ID"), "C0987654321");
    await user.click(screen.getByRole("button", { name: /^enroll$/i }));
    await waitFor(() =>
      expect(setEntry).toHaveBeenCalledWith({
        automationId: "auto-slack",
        inputKey: "channels",
        entryKey: "C0987654321",
        valueJson: JSON.stringify("prof-a"),
        enable: true,
      }),
    );
  });

  it("removes a channel via the confirm dialog", async () => {
    renderWithProviders(<SlackThreads />);
    const user = userEvent.setup();
    await screen.findByText("C0123456789");
    await user.click(screen.getByRole("button", { name: /^remove$/i }));
    await user.click(await screen.findByRole("button", { name: /^remove$/i }));
    await waitFor(() =>
      expect(setEntry).toHaveBeenCalledWith({
        automationId: "auto-slack",
        inputKey: "channels",
        entryKey: "C0123456789",
        enable: false,
      }),
    );
  });
});
