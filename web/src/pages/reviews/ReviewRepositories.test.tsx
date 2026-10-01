import { describe, it, expect, vi, beforeEach } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "../../test-utils";
import { ReviewRepositories } from "./ReviewRepositories";

const upsert = vi.hoisted(() => vi.fn().mockResolvedValue({}));
const remove = vi.hoisted(() => vi.fn());
const setInput = vi.hoisted(() => vi.fn().mockResolvedValue({}));
const state = vi.hoisted(() => ({
  enrollments: [] as Array<{
    repo: string;
    triggerMode: string;
    autofix: string;
  }>,
  profiles: [] as Array<{ id: string; name: string }>,
  inputs: {} as Record<string, unknown>,
}));

vi.mock("../../hooks/useEnrollments", () => ({
  useEnrollments: () => ({ data: { enrollments: state.enrollments }, isPending: false }),
  useUpsertEnrollment: () => ({ mutateAsync: upsert, isPending: false }),
  useDeleteEnrollment: () => ({ mutate: remove, isPending: false }),
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
}));

beforeEach(() => {
  upsert.mockClear();
  remove.mockClear();
  setInput.mockClear();
  state.enrollments = [{ repo: "cortexapps/engrams", triggerMode: "manual", autofix: "off" }];
  state.profiles = [{ id: "p1", name: "Reviewer" }];
  state.inputs = { repos: {}, profile: "p1" };
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

  it("enrolls a new repo through the dialog", async () => {
    renderWithProviders(<ReviewRepositories />);
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /enroll repo/i }));
    await user.type(await screen.findByLabelText("Repository"), "cortexapps/backend");
    await user.click(screen.getByRole("button", { name: /^enroll$/i }));
    await waitFor(() =>
      expect(upsert).toHaveBeenCalledWith({
        repo: "cortexapps/backend",
        triggerMode: "manual",
        autofix: "off",
      }),
    );
  });

  it("un-enrolls a repo via the confirm dialog", async () => {
    renderWithProviders(<ReviewRepositories />);
    const user = userEvent.setup();
    await screen.findByText("cortexapps/engrams");
    await user.click(screen.getByRole("button", { name: /^remove$/i }));
    await user.click(await screen.findByRole("button", { name: /^remove$/i }));
    await waitFor(() => expect(remove).toHaveBeenCalledWith({ repo: "cortexapps/engrams" }));
  });
});
