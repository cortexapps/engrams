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
// The harness/model/effort dropdowns read the catalog; mock the hook so the
// editor needs no QueryClient/transport (ADR 0062/0063).
vi.mock("../../hooks/useHarnessCatalog", () => ({
  useHarnessCatalog: () => ({
    data: [
      {
        name: "claude",
        descriptor: {
          label: "Claude Code",
          models: [
            { id: "opus", label: "Claude Opus 4.8", default: true, env: {} },
            { id: "sonnet", label: "Claude Sonnet 4.6", default: false, env: {} },
          ],
          effort: [{ id: "high", label: "High", default: false, env: {} }],
        },
      },
    ],
  }),
}));
const uploadSkill = vi.hoisted(() => vi.fn().mockResolvedValue({ skill: { name: "x" } }));
vi.mock("../../hooks/useSkills", () => ({
  useSkills: () => ({
    data: [
      { name: "playwright", label: "Browser (Playwright)", description: "browser", builtin: true },
      { name: "my-linter", label: "my-linter", description: "lint", builtin: false },
    ],
  }),
  useUploadSkill: () => ({ mutateAsync: uploadSkill, isPending: false }),
}));
vi.mock("../../hooks/useOrgSecrets", () => ({ useOrgSecretNames: () => ({ data: [] }) }));
// The editor derives its policy rail + connected-connector cards from the joined
// catalog; mock it so the test needs no QueryClient/transport.
vi.mock("../../components/integrations/useConnectorViews", () => ({
  useConnectorViews: () => ({ views: [], isLoading: false, error: null }),
}));
vi.mock("@tanstack/react-router", async (orig) => ({
  ...(await orig()),
  useNavigate: () => vi.fn(),
  useParams: () => ({}),
  Link: ({
    children,
    to: _to,
    params: _params,
    ...rest
  }: Record<string, unknown> & { children: React.ReactNode }) => <a {...rest}>{children}</a>,
}));

beforeEach(() => {
  profileHolder.value = undefined;
  create.mockClear();
  update.mockClear();
  uploadSkill.mockClear();
});

const openAdvanced = () => fireEvent.click(screen.getByRole("button", { name: /advanced/i }));

describe("SessionProfileEditor (create)", () => {
  it("requires a name, then calls createProfile with the default image", async () => {
    render(<SessionProfileEditor mode="create" />);
    fireEvent.change(screen.getByLabelText(/profile name/i), {
      target: { value: "Backend Agent" },
    });
    fireEvent.click(screen.getByRole("button", { name: /create profile/i }));
    await waitFor(() => expect(create).toHaveBeenCalled());
    expect(create.mock.calls[0][0]).toMatchObject({ name: "Backend Agent", imageId: "i1" });
  });

  // ADR 0063: a profile ALWAYS names a concrete harness (no "inherit deployment
  // default") — a new profile defaults to the first registered harness, so the
  // create payload carries it (and the Model/Effort selectors have a descriptor).
  it("defaults a new profile to the first registered harness (never null)", async () => {
    render(<SessionProfileEditor mode="create" />);
    fireEvent.change(screen.getByLabelText(/profile name/i), {
      target: { value: "Harnessed" },
    });
    fireEvent.click(screen.getByRole("button", { name: /create profile/i }));
    await waitFor(() => expect(create).toHaveBeenCalled());
    expect(create.mock.calls[0][0]).toMatchObject({ harness: "claude" });
  });

  it("includes the deny-default network + extra allowed hosts in the payload (ADR 0057)", async () => {
    render(<SessionProfileEditor mode="create" />);
    fireEvent.change(screen.getByLabelText(/profile name/i), { target: { value: "Net Agent" } });
    fireEvent.click(screen.getByRole("button", { name: /add extra hosts/i }));
    fireEvent.change(screen.getByLabelText(/allowed hosts/i), {
      target: { value: "sentry.io\napi.github.com" },
    });
    fireEvent.click(screen.getByRole("button", { name: /create profile/i }));
    await waitFor(() => expect(create).toHaveBeenCalled());
    expect(create.mock.calls[0][0].network).toMatchObject({
      default: "deny",
      allowHosts: ["sentry.io", "api.github.com"],
    });
  });

  it("toggling Default includes is_default in the create payload (ADR 0060)", async () => {
    render(<SessionProfileEditor mode="create" />);
    fireEvent.change(screen.getByLabelText(/profile name/i), {
      target: { value: "Default Agent" },
    });
    fireEvent.click(screen.getByLabelText(/default profile/i));
    fireEvent.click(screen.getByRole("button", { name: /create profile/i }));
    await waitFor(() => expect(create).toHaveBeenCalled());
    expect(create.mock.calls[0][0]).toMatchObject({ isDefault: true });
  });

  it("leaves is_default false when the toggle is untouched", async () => {
    render(<SessionProfileEditor mode="create" />);
    fireEvent.change(screen.getByLabelText(/profile name/i), { target: { value: "Plain Agent" } });
    fireEvent.click(screen.getByRole("button", { name: /create profile/i }));
    await waitFor(() => expect(create).toHaveBeenCalled());
    expect(create.mock.calls[0][0]).toMatchObject({ isDefault: false });
  });

  it("toggling a skill includes it in the payload (ADR 0055)", async () => {
    render(<SessionProfileEditor mode="create" />);
    fireEvent.change(screen.getByLabelText(/profile name/i), {
      target: { value: "Browser Agent" },
    });
    openAdvanced();
    fireEvent.click(screen.getByTestId("skill-playwright"));
    fireEvent.click(screen.getByRole("button", { name: /create profile/i }));
    await waitFor(() => expect(create).toHaveBeenCalled());
    expect(create.mock.calls[0][0].skills).toEqual(["playwright"]);
  });

  it("adding a port includes portExposures in the create payload (ADR 0064 P4)", async () => {
    render(<SessionProfileEditor mode="create" />);
    fireEvent.change(screen.getByLabelText(/profile name/i), {
      target: { value: "Dev Server Agent" },
    });
    openAdvanced();
    fireEvent.change(screen.getByTestId("port-add-input"), { target: { value: "3000" } });
    fireEvent.click(screen.getByTestId("port-add-btn"));
    expect(screen.getByTestId("port-chip-3000")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /create profile/i }));
    await waitFor(() => expect(create).toHaveBeenCalled());
    expect(create.mock.calls[0][0].portExposures).toEqual([3000]);
  });

  it("rejects an out-of-range port without adding a chip (ADR 0064 P4)", () => {
    render(<SessionProfileEditor mode="create" />);
    openAdvanced();
    fireEvent.change(screen.getByTestId("port-add-input"), { target: { value: "0" } });
    fireEvent.click(screen.getByTestId("port-add-btn"));
    expect(screen.getByTestId("port-add-error")).toBeTruthy();
    expect(screen.queryByTestId("port-chip-0")).toBeNull();
  });

  it("renders an uploaded catalog skill as a selectable toggle (ADR 0055 P2)", () => {
    render(<SessionProfileEditor mode="create" />);
    openAdvanced();
    expect(screen.getByTestId("skill-my-linter")).toBeTruthy();
  });

  it("uploads a SKILL.md via the inline control (ADR 0055 P2)", async () => {
    render(<SessionProfileEditor mode="create" />);
    openAdvanced();
    fireEvent.change(screen.getByTestId("skill-upload-name"), { target: { value: "my-skill" } });
    const file = new File(["# Hi\n"], "SKILL.md", { type: "text/markdown" });
    fireEvent.change(screen.getByTestId("skill-upload-file"), { target: { files: [file] } });
    fireEvent.click(screen.getByTestId("skill-upload-submit"));
    await waitFor(() => expect(uploadSkill).toHaveBeenCalled());
    expect(uploadSkill.mock.calls[0][0]).toMatchObject({ name: "my-skill" });
  });
});

describe("SessionProfileEditor (edit)", () => {
  it("hydrates from the existing profile in edit mode", async () => {
    profileHolder.value = {
      profile: {
        id: "p1",
        name: "Backend Agent",
        description: "Node API",
        icon: "Server",
        imageId: "i1",
        includeUserTokens: true,
        envVars: { ANTHROPIC_MODEL: "claude-x" },
        capabilities: [],
        skills: [],
      },
    };
    render(<SessionProfileEditor mode="edit" />);
    const nameInput = await screen.findByLabelText(/profile name/i);
    await waitFor(() => expect((nameInput as HTMLInputElement).value).toBe("Backend Agent"));
    // env vars live under Advanced; opening it reveals the hydrated KEY input.
    fireEvent.click(screen.getByRole("button", { name: /advanced/i }));
    expect((screen.getByDisplayValue("ANTHROPIC_MODEL") as HTMLInputElement).value).toBe(
      "ANTHROPIC_MODEL",
    );
  });
});
