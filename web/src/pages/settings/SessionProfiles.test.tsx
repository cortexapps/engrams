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
              capabilities: [],
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
              capabilities: [],
              archived: false,
            },
          ],
    },
    isPending: false,
  }),
  useDeleteProfile: () => ({ mutateAsync: del, isPending: false }),
}));
vi.mock("../../hooks/useEnabledImages", () => ({
  useEnabledImages: () => ({
    data: [{ id: "i", image_uri: "registry/api:latest" }],
    isLoading: false,
  }),
}));
vi.mock("../../components/integrations/useConnectorViews", () => ({
  useConnectorViews: () => ({ views: [], isLoading: false, error: null }),
}));

beforeEach(() => del.mockClear());

describe("SessionProfiles list", () => {
  it("renders active profiles with a create affordance", async () => {
    renderWithProviders(<SessionProfiles />);
    expect(await screen.findByText("Backend")).toBeTruthy();
    expect(screen.getByText("Node API")).toBeTruthy();
    // Capability-less profile reads as fully sandboxed in the meta strip.
    expect(screen.getAllByText(/fully sandboxed/i).length).toBeGreaterThanOrEqual(1);
    expect(screen.getByRole("link", { name: /new profile/i })).toBeTruthy();
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
