import { describe, it, expect, vi, beforeEach } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "../../test-utils";
import { SessionProfiles } from "./SessionProfiles";

const del = vi.hoisted(() => vi.fn().mockResolvedValue({}));
vi.mock("../../hooks/useProfiles", () => ({
  useProfiles: (includeArchived: boolean) => ({
    data: {
      profiles: includeArchived
        ? [
            {
              id: "a1",
              name: "Old",
              description: "",
              icon: "Bot",
              imageId: "i",
              includeUserTokens: false,
              envVars: {},
              archived: true,
            },
          ]
        : [
            {
              id: "p1",
              name: "Backend",
              description: "Node API",
              icon: "Server",
              imageId: "i",
              includeUserTokens: true,
              envVars: {},
              archived: false,
            },
          ],
    },
    isPending: false,
  }),
  useDeleteProfile: () => ({ mutateAsync: del, isPending: false }),
}));

beforeEach(() => {
  del.mockClear();
});

describe("SessionProfiles list", () => {
  it("renders active profiles with a create affordance", async () => {
    renderWithProviders(<SessionProfiles />);
    expect(await screen.findByText("Backend")).toBeTruthy();
    expect(screen.getByText("carries your token")).toBeTruthy();
    expect(screen.getByRole("link", { name: /create profile/i })).toBeTruthy();
  });

  it("archives an active profile via the confirm dialog", async () => {
    renderWithProviders(<SessionProfiles />);
    const user = userEvent.setup();
    await screen.findByText("Backend");
    await user.click(screen.getByRole("button", { name: /^archive$/i }));
    await user.click(await screen.findByRole("button", { name: /archive profile/i }));
    await waitFor(() => expect(del).toHaveBeenCalledWith({ id: "p1" }));
  });
});
