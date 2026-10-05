import { describe, it, expect, vi, beforeEach } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "../../test-utils";
import { ReviewRepositories } from "./ReviewRepositories";

const setEntry = vi.hoisted(() => vi.fn().mockResolvedValue({}));
const setInput = vi.hoisted(() => vi.fn().mockResolvedValue({}));
const state = vi.hoisted(() => ({
  profiles: [] as Array<{ id: string; name: string }>,
  inputs: {} as Record<string, unknown>,
}));

vi.mock("../../hooks/useProfiles", () => ({
  useProfiles: () => ({ data: { profiles: state.profiles } }),
}));
vi.mock("../../hooks/useAutomations", () => ({
  useBuiltinAutomation: () => ({
    data: { automation: { id: "auto-pr", inputsJson: JSON.stringify(state.inputs) } },
    isPending: false,
  }),
  useSetInputValue: () => ({ mutateAsync: setInput, isPending: false }),
  // The list IS the automation's `repos` input; enrolling writes one entry.
  useSetMapInputEntry: () => ({ mutateAsync: setEntry, mutate: setEntry, isPending: false }),
}));

beforeEach(() => {
  setEntry.mockClear();
  setInput.mockClear();
  state.profiles = [{ id: "p1", name: "Reviewer" }];
  state.inputs = {
    repos: { "cortexapps/engrams": { mode: "on_request", autofix: false } },
    profile: "p1",
  };
});

describe("ReviewRepositories", () => {
  it("lists enrolled repos with their trigger mode and the link to the automation", async () => {
    renderWithProviders(<ReviewRepositories />);
    expect(await screen.findByText("cortexapps/engrams")).toBeTruthy();
    expect(screen.getByText("@mention")).toBeTruthy();
    expect(screen.getByRole("link", { name: /PR review automation/ })).toBeTruthy();
    // A reviewer profile is picked → no warning banner.
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("warns when no reviewer profile is picked, and picking one writes the automation's input", async () => {
    state.inputs = { repos: {}, profile: "" };
    renderWithProviders(<ReviewRepositories />);
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toMatch(/No reviewer profile/);
    const user = userEvent.setup();
    await user.click(screen.getByRole("combobox", { name: /reviewer profile/i }));
    await user.click(await screen.findByRole("option", { name: "Reviewer" }));
    await waitFor(() =>
      expect(setInput).toHaveBeenCalledWith({
        automationId: "auto-pr",
        inputKey: "profile",
        valueJson: JSON.stringify("p1"),
      }),
    );
  });

  it("enrolls a new repo as one entry of the automation's repos map, enabling it", async () => {
    renderWithProviders(<ReviewRepositories />);
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /enroll repo/i }));
    await user.type(await screen.findByLabelText("Repository"), "cortexapps/backend");
    await user.click(screen.getByRole("button", { name: /^enroll$/i }));
    await waitFor(() =>
      expect(setEntry).toHaveBeenCalledWith({
        automationId: "auto-pr",
        inputKey: "repos",
        entryKey: "cortexapps/backend",
        valueJson: JSON.stringify({ mode: "on_request", autofix: false }),
        enable: true,
      }),
    );
  });

  it("editing a repo rewrites its entry without turning a paused automation back on", async () => {
    renderWithProviders(<ReviewRepositories />);
    const user = userEvent.setup();
    await screen.findByText("cortexapps/engrams");
    await user.click(screen.getByRole("button", { name: /^edit$/i }));
    await user.click(await screen.findByRole("combobox", { name: /trigger/i }));
    await user.click(await screen.findByRole("option", { name: /^auto/i }));
    await user.click(screen.getByRole("button", { name: /^save$/i }));
    await waitFor(() =>
      expect(setEntry).toHaveBeenCalledWith({
        automationId: "auto-pr",
        inputKey: "repos",
        entryKey: "cortexapps/engrams",
        valueJson: JSON.stringify({ mode: "auto", autofix: false }),
        enable: false,
      }),
    );
  });

  it("un-enrolls a repo by removing its map entry, via the confirm dialog", async () => {
    renderWithProviders(<ReviewRepositories />);
    const user = userEvent.setup();
    await screen.findByText("cortexapps/engrams");
    await user.click(screen.getByRole("button", { name: /^remove$/i }));
    await user.click(await screen.findByRole("button", { name: /^remove$/i }));
    await waitFor(() =>
      expect(setEntry).toHaveBeenCalledWith({
        automationId: "auto-pr",
        inputKey: "repos",
        entryKey: "cortexapps/engrams",
        enable: false,
      }),
    );
  });
});
