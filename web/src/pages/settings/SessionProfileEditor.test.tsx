import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { SessionProfileEditor } from "./SessionProfileEditor";

const profileHolder = vi.hoisted(() => ({ value: undefined as undefined | { profile: unknown } }));
const create = vi.hoisted(() => vi.fn().mockResolvedValue({ profile: { id: "new" } }));
const update = vi.hoisted(() => vi.fn().mockResolvedValue({ profile: { id: "p1" } }));
vi.mock("../../hooks/useProfiles", () => ({
  useProfile: () => ({ data: profileHolder.value, isPending: false }),
  useCreateProfile: () => ({ mutateAsync: create, isPending: false }),
  useUpdateProfile: () => ({ mutateAsync: update, isPending: false }),
}));
vi.mock("../../hooks/useEnabledImages", () => ({
  useEnabledImages: () => ({
    data: [{ id: "i1", image_uri: "registry/api:latest" }],
    isLoading: false,
  }),
}));
const uploadSkill = vi.hoisted(() => vi.fn().mockResolvedValue({ skill: { name: "x" } }));
vi.mock("../../hooks/useSkills", () => ({
  useSkills: () => ({
    data: [
      { name: "skills", label: "Built-in skills", description: "wrappers", builtin: true },
      { name: "playwright", label: "Browser (Playwright)", description: "browser", builtin: true },
      { name: "my-linter", label: "my-linter", description: "lint", builtin: false },
    ],
  }),
  useUploadSkill: () => ({ mutateAsync: uploadSkill, isPending: false }),
}));
vi.mock("@tanstack/react-router", async (orig) => ({
  ...(await orig()),
  useNavigate: () => vi.fn(),
  useParams: () => ({}),
}));

beforeEach(() => {
  profileHolder.value = undefined;
  create.mockClear();
  update.mockClear();
  uploadSkill.mockClear();
});

describe("SessionProfileEditor (create)", () => {
  it("requires a name and image, then calls createProfile", async () => {
    render(<SessionProfileEditor mode="create" />);
    fireEvent.change(screen.getByLabelText(/name/i), { target: { value: "Backend Agent" } });
    fireEvent.click(screen.getByRole("button", { name: /create profile/i }));
    await waitFor(() => expect(create).toHaveBeenCalled());
    expect(create.mock.calls[0][0]).toMatchObject({ name: "Backend Agent", imageId: "i1" });
  });

  it("toggling a built-in skill includes it in the createProfile payload (ADR 0055)", async () => {
    render(<SessionProfileEditor mode="create" />);
    fireEvent.change(screen.getByLabelText(/name/i), { target: { value: "Browser Agent" } });
    fireEvent.click(screen.getByTestId("skill-playwright"));
    fireEvent.click(screen.getByRole("button", { name: /create profile/i }));
    await waitFor(() => expect(create).toHaveBeenCalled());
    expect(create.mock.calls[0][0].skills).toEqual(["playwright"]);
  });

  it("renders an uploaded catalog skill as a selectable toggle (ADR 0055 P2)", () => {
    render(<SessionProfileEditor mode="create" />);
    // getByTestId throws if absent, so a truthy assertion is the check.
    expect(screen.getByTestId("skill-my-linter")).toBeTruthy();
  });

  it("uploads a SKILL.md via the inline control (ADR 0055 P2)", async () => {
    render(<SessionProfileEditor mode="create" />);
    fireEvent.change(screen.getByTestId("skill-upload-name"), { target: { value: "my-skill" } });
    const file = new File(["# Hi\n"], "SKILL.md", { type: "text/markdown" });
    fireEvent.change(screen.getByTestId("skill-upload-file"), { target: { files: [file] } });
    fireEvent.click(screen.getByTestId("skill-upload-submit"));
    await waitFor(() => expect(uploadSkill).toHaveBeenCalled());
    expect(uploadSkill.mock.calls[0][0]).toMatchObject({ name: "my-skill" });
  });
});

describe("SessionProfileEditor (edit)", () => {
  it("hydrates the form from the existing profile in edit mode", async () => {
    profileHolder.value = {
      profile: {
        id: "p1",
        name: "Backend Agent",
        description: "Node API",
        icon: "Server",
        imageId: "i1",
        includeUserTokens: true,
        envVars: { ANTHROPIC_MODEL: "claude-x" },
      },
    };
    render(<SessionProfileEditor mode="edit" />);
    // The hydration useEffect resets the form from existing.profile.
    const nameInput = await screen.findByLabelText(/name/i);
    await waitFor(() => expect((nameInput as HTMLInputElement).value).toBe("Backend Agent"));
    // mapToEnvRows populated a KEY input with the env var key.
    expect((screen.getByDisplayValue("ANTHROPIC_MODEL") as HTMLInputElement).value).toBe(
      "ANTHROPIC_MODEL",
    );
  });
});
