import { describe, it, expect, vi } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { SessionProfileEditor } from "./SessionProfileEditor";

const create = vi.fn().mockResolvedValue({ profile: { id: "new" } });
vi.mock("../../hooks/useProfiles", () => ({
  useProfile: () => ({ data: undefined, isPending: false }),
  useCreateProfile: () => ({ mutateAsync: create, isPending: false }),
  useUpdateProfile: () => ({ mutateAsync: vi.fn(), isPending: false }),
}));
vi.mock("../../hooks/useEnabledImages", () => ({
  useEnabledImages: () => ({
    data: [{ id: "i1", image_uri: "registry/api:latest" }],
    isLoading: false,
  }),
}));
vi.mock("@tanstack/react-router", async (orig) => ({
  ...(await orig()),
  useNavigate: () => vi.fn(),
  useParams: () => ({}),
}));

describe("SessionProfileEditor (create)", () => {
  it("requires a name and image, then calls createProfile", async () => {
    render(<SessionProfileEditor mode="create" />);
    fireEvent.change(screen.getByLabelText(/name/i), { target: { value: "Backend Agent" } });
    fireEvent.click(screen.getByRole("button", { name: /create profile/i }));
    await waitFor(() => expect(create).toHaveBeenCalled());
    expect(create.mock.calls[0][0]).toMatchObject({ name: "Backend Agent", imageId: "i1" });
  });
});
