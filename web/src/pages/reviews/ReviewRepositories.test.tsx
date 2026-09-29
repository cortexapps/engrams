import { describe, it, expect, vi, beforeEach } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "../../test-utils";
import { ReviewRepositories } from "./ReviewRepositories";

const upsert = vi.hoisted(() => vi.fn().mockResolvedValue({}));
const remove = vi.hoisted(() => vi.fn());
const state = vi.hoisted(() => ({
  enrollments: [] as Array<{
    repo: string;
    triggerMode: string;
    autofix: string;
    engine: string;
  }>,
  profiles: [] as Array<{ id: string; name: string; designation?: string }>,
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
  useBuiltinAutomation: () => ({ data: { automation: { id: "auto-pr" } } }),
}));

beforeEach(() => {
  upsert.mockClear();
  remove.mockClear();
  state.enrollments = [
    { repo: "cortexapps/engrams", triggerMode: "manual", autofix: "off", engine: "automation" },
  ];
  state.profiles = [{ id: "p1", name: "Reviewer", designation: "pr_reviewer" }];
});

describe("ReviewRepositories", () => {
  it("lists enrolled repos with their trigger mode, engine, and the link to the automation", async () => {
    renderWithProviders(<ReviewRepositories />);
    expect(await screen.findByText("cortexapps/engrams")).toBeTruthy();
    expect(screen.getByText("@mention")).toBeTruthy();
    expect(screen.getByText("automation")).toBeTruthy();
    expect(screen.getByRole("link", { name: /PR review automation/ })).toBeTruthy();
    // A pr_reviewer profile exists → no warning banner.
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("warns when no profile is designated pr_reviewer", async () => {
    state.profiles = [{ id: "p1", name: "Backend" }];
    renderWithProviders(<ReviewRepositories />);
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toMatch(/pr_reviewer/);
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

  it("marks a legacy row and offers to move it on save", async () => {
    state.enrollments = [
      { repo: "cortexapps/brain-backend", triggerMode: "manual", autofix: "off", engine: "legacy" },
    ];
    renderWithProviders(<ReviewRepositories />);
    expect(await screen.findByText("legacy")).toBeTruthy();
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: /^edit$/i }));
    expect(
      await screen.findByRole("button", { name: /save and move to automation/i }),
    ).toBeTruthy();
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
