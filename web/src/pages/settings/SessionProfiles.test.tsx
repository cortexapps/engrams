import { describe, it, expect, vi } from "vitest";
import { screen } from "@testing-library/react";
import { renderWithProviders } from "../../test-utils";
import { SessionProfiles } from "./SessionProfiles";

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
  useDeleteProfile: () => ({ mutateAsync: vi.fn(), isPending: false }),
}));

describe("SessionProfiles list", () => {
  it("renders active profiles with a create affordance", async () => {
    renderWithProviders(<SessionProfiles />);
    expect(await screen.findByText("Backend")).toBeTruthy();
    expect(screen.getByText("carries your token")).toBeTruthy();
    expect(screen.getByRole("link", { name: /create profile/i })).toBeTruthy();
  });
});
