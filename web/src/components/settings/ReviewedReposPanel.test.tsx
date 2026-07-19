import { describe, it, expect, vi, beforeEach } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "../../test-utils";
import { ReviewedReposPanel } from "./ReviewedReposPanel";

const upsert = vi.hoisted(() => vi.fn().mockResolvedValue({}));
const remove = vi.hoisted(() => vi.fn());
const state = vi.hoisted(() => ({
  enrollments: [] as Array<{
    repo: string;
    triggerMode: string;
    autofix: string;
    profileId: string;
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

beforeEach(() => {
  upsert.mockClear();
  remove.mockClear();
  state.enrollments = [
    { repo: "cortexapps/engrams", triggerMode: "manual", autofix: "off", profileId: "" },
  ];
  state.profiles = [{ id: "p1", name: "Reviewer", designation: "pr_reviewer" }];
});

describe("ReviewedReposPanel", () => {
  it("lists enrolled repos with their trigger mode", async () => {
    renderWithProviders(<ReviewedReposPanel />);
    expect(await screen.findByText("cortexapps/engrams")).toBeTruthy();
    expect(screen.getByText("@mention")).toBeTruthy();
    // A pr_reviewer profile exists → no warning banner.
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("warns when no profile is designated pr_reviewer", async () => {
    state.profiles = [{ id: "p1", name: "Backend" }];
    renderWithProviders(<ReviewedReposPanel />);
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toMatch(/pr_reviewer/);
  });

  it("enrolls a new repo through the dialog", async () => {
    renderWithProviders(<ReviewedReposPanel />);
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /enroll repo/i }));
    await user.type(await screen.findByLabelText("Repository"), "cortexapps/backend");
    await user.click(screen.getByRole("button", { name: /^enroll$/i }));
    await waitFor(() =>
      expect(upsert).toHaveBeenCalledWith({
        repo: "cortexapps/backend",
        triggerMode: "manual",
        autofix: "off",
        profileId: "",
      }),
    );
  });

  it("un-enrolls a repo via the confirm dialog", async () => {
    renderWithProviders(<ReviewedReposPanel />);
    const user = userEvent.setup();
    await screen.findByText("cortexapps/engrams");
    await user.click(screen.getByRole("button", { name: /^remove$/i }));
    await user.click(await screen.findByRole("button", { name: /^remove$/i }));
    await waitFor(() => expect(remove).toHaveBeenCalledWith({ repo: "cortexapps/engrams" }));
  });
});
