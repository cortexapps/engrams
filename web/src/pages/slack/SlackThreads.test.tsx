import { describe, it, expect, vi, beforeEach } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "../../test-utils";
import { SlackThreads, channelsOf, inputsOf } from "./SlackThreads";

const setEntry = vi.hoisted(() => vi.fn().mockResolvedValue({}));
const setValue = vi.hoisted(() => vi.fn().mockResolvedValue({}));
const setEnabled = vi.hoisted(() => vi.fn().mockResolvedValue({}));
const state = vi.hoisted(() => ({
  inputs: { channels: {} as Record<string, string>, default_profile: "" },
  enabled: false,
  profiles: [] as Array<{ id: string; name: string }>,
  instances: [] as Array<Record<string, unknown>>,
}));

vi.mock("sonner", () => ({ toast: { success: () => {}, error: () => {} } }));
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
  useSetInputValue: () => ({ mutateAsync: setValue, isPending: false }),
  useSetAutomationEnabled: () => ({ mutateAsync: setEnabled, isPending: false }),
}));
vi.mock("../../hooks/useInstances", () => ({
  useInstanceList: () => ({ data: { instances: state.instances }, isPending: false }),
  useRecentDrops: () => ({ data: { drops: [] } }),
  useInstance: () => ({ data: undefined }),
}));
vi.mock("../../hooks/useAutomationRuns", () => ({
  useRunList: () => ({ data: { runs: [] } }),
}));
vi.mock("../../hooks/useAutomationInputs", () => ({
  useInputKeyOptions: () => ({ data: { options: [{ key: "C0123456789", label: "#alerts" }] } }),
}));
vi.mock("../../hooks/useProfiles", () => ({
  useProfiles: () => ({ data: { profiles: state.profiles } }),
}));
vi.mock("../../hooks/useNow", () => ({ useNow: () => Date.parse("2026-09-30T12:00:00Z") }));

beforeEach(() => {
  setEntry.mockClear();
  setValue.mockClear();
  setEnabled.mockClear();
  state.inputs = { channels: { C0123456789: "prof-a" }, default_profile: "prof-d" };
  state.enabled = true;
  state.profiles = [
    { id: "prof-d", name: "Helpdesk" },
    { id: "prof-a", name: "Alerts" },
  ];
  state.instances = [
    {
      id: "ai_1",
      automationId: "auto-slack",
      key: "T1:C0123456789:1700.1",
      label: "can you draw a pelican?",
      status: "open",
      inputsJson: "{}",
      openedBy: "event:slack:Ev1",
      openedAt: "2026-09-30T11:00:00Z",
    },
  ];
});

describe("channelsOf / inputsOf", () => {
  it("read the stored inputs and tolerate junk", () => {
    expect(channelsOf(JSON.stringify({ channels: { C2: "p", C1: "q" } }))).toEqual([
      { channel: "C1", profileId: "q" },
      { channel: "C2", profileId: "p" },
    ]);
    expect(channelsOf("nope")).toEqual([]);
    expect(inputsOf(JSON.stringify({ default_profile: "x" }))).toEqual({ default_profile: "x" });
    expect(inputsOf("nope")).toEqual({});
  });
});

describe("SlackThreads", () => {
  it("shows the switch on, the default profile, the overrides, the threads, and the automation link", async () => {
    renderWithProviders(<SlackThreads />);
    expect(await screen.findByRole("switch", { name: /pause answering/i })).toBeTruthy();
    expect(screen.getByText("Answering @-mentions")).toBeTruthy();
    expect(screen.getByText("C0123456789")).toBeTruthy();
    expect(screen.getByText("Alerts")).toBeTruthy();
    expect(screen.getByRole("link", { name: /Slack threads automation/ })).toBeTruthy();
    // A thread row reads as a human would: the opening mention as the title,
    // the named channel and thread as the place — never a bare key.
    expect(await screen.findByText("can you draw a pelican?")).toBeTruthy();
    expect(screen.getAllByText("#alerts · thread 1700.1").length).toBeGreaterThan(0);
    expect(screen.queryByText("T1:C0123456789:1700.1")).toBeNull();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("cannot be turned on with no profile anywhere, and warns when on with none", async () => {
    state.inputs = { channels: {}, default_profile: "" };
    state.enabled = false;
    const { unmount } = renderWithProviders(<SlackThreads />);
    const sw = await screen.findByRole("switch", { name: /answer @-mentions/i });
    expect((sw as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByText(/Pick a default profile to turn answering on/)).toBeTruthy();
    unmount();

    state.enabled = true;
    renderWithProviders(<SlackThreads />);
    expect((await screen.findByRole("alert")).textContent).toMatch(/no profile is set/);
  });

  it("changing the default profile writes that one key, never the whole inputs blob", async () => {
    renderWithProviders(<SlackThreads />);
    const user = userEvent.setup();
    await user.click(await screen.findByRole("combobox", { name: "Default profile" }));
    await user.click(await screen.findByRole("option", { name: "Alerts" }));
    await waitFor(() =>
      expect(setValue).toHaveBeenCalledWith({
        automationId: "auto-slack",
        inputKey: "default_profile",
        valueJson: JSON.stringify("prof-a"),
      }),
    );
  });

  it("the switch pauses and resumes the automation", async () => {
    renderWithProviders(<SlackThreads />);
    const user = userEvent.setup();
    await user.click(await screen.findByRole("switch", { name: /pause answering/i }));
    await waitFor(() =>
      expect(setEnabled).toHaveBeenCalledWith({ id: "auto-slack", enabled: false }),
    );
  });

  it("adds a channel override as one map entry without enabling", async () => {
    renderWithProviders(<SlackThreads />);
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /add override/i }));
    await user.type(await screen.findByLabelText("Channel ID"), "C0987654321");
    await user.click(screen.getByRole("button", { name: /^add$/i }));
    await waitFor(() =>
      expect(setEntry).toHaveBeenCalledWith({
        automationId: "auto-slack",
        inputKey: "channels",
        entryKey: "C0987654321",
        valueJson: JSON.stringify("prof-d"),
        enable: false,
      }),
    );
  });

  it("removes an override via the confirm dialog", async () => {
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
