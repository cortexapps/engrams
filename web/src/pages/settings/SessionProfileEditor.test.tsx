import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { SessionProfileEditor } from "./SessionProfileEditor";

const profileHolder = vi.hoisted(() => ({ value: undefined as undefined | { profile: unknown } }));
const paramsHolder = vi.hoisted(() => ({ value: {} as { id?: string } }));
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
      { name: "browser", label: "Browser", description: "browser", builtin: true },
      { name: "my-linter", label: "my-linter", description: "lint", builtin: false },
    ],
  }),
  useUploadSkill: () => ({ mutateAsync: uploadSkill, isPending: false }),
}));
const orgSecretNamesHolder = vi.hoisted(() => ({ value: [] as string[] }));
vi.mock("../../hooks/useOrgSecrets", () => ({
  useOrgSecretNames: () => ({ data: orgSecretNamesHolder.value }),
}));
const connectionsHolder = vi.hoisted(() => ({ value: [] as unknown[] }));
vi.mock("../../hooks/useIntegrations", () => ({
  useIntegrationConnections: () => ({ data: { connections: connectionsHolder.value } }),
}));
// The editor derives its policy rail, its connected-connector cards AND — since
// the provider seam — the named-connection blocks from the joined catalog. Mock
// only the hook, keeping the real helpers, and serve the Google entry the
// catalog now returns instead of the entry the web used to synthesize.
const GOOGLE_CATALOG_VIEW = {
  provider: "gcp",
  defaultConnectionId: "",
  name: "Google Cloud",
  category: "cloud",
  blurb: "Call Google Cloud APIs with a short-lived, policy-bound credential.",
  icon: { mono: "GC", color: "#4285f4" },
  credentialSource: "mint" as const,
  hosts: ["compute.googleapis.com", "logging.googleapis.com"],
  capabilities: [
    {
      action: "compute.instances.get",
      access: "read" as const,
      label: "Describe Compute Engine instances",
      host: "compute.googleapis.com",
    },
    {
      action: "compute.instances.start",
      access: "write" as const,
      label: "Start Compute Engine instances",
      host: "compute.googleapis.com",
    },
    {
      action: "logging.entries.list",
      access: "read" as const,
      label: "Read Cloud Logging entries",
      host: "logging.googleapis.com",
    },
    {
      action: "api.call",
      access: "write" as const,
      label: "Call configured Google APIs",
      endpointRule: "google-api" as const,
    },
    {
      action: "gke.api.call",
      access: "write" as const,
      label: "Call the configured GKE API server",
      endpointRule: "non-google-api" as const,
    },
  ],
  status: "connected" as const,
  builtin: true,
  usedBy: 0,
  usedByProfiles: [],
  connectionModel: "named" as const,
};
vi.mock("../../components/integrations/useConnectorViews", async (orig) => ({
  ...(await orig<object>()),
  useConnectorViews: () => ({
    views: [GOOGLE_CATALOG_VIEW],
    isLoading: false,
    error: null,
  }),
}));
vi.mock("@tanstack/react-router", async (orig) => ({
  ...(await orig()),
  useNavigate: () => vi.fn(),
  useParams: () => paramsHolder.value,
  Link: ({
    children,
    to: _to,
    params: _params,
    ...rest
  }: Record<string, unknown> & { children: React.ReactNode }) => <a {...rest}>{children}</a>,
}));

beforeEach(() => {
  profileHolder.value = undefined;
  paramsHolder.value = {};
  connectionsHolder.value = [];
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

  // Regression: the collection editors have no registered input behind them, so
  // they must ride useController. With a bare watch/setValue pair, react-hook-form
  // kept ADDING a row (the array's length changed) but silently dropped an
  // in-place edit — picking an org secret left `ref` empty, all the way to the
  // wire. This drives the real path: add a row, pick a ref, save.
  it("carries an org secret picked in a secret row through to the payload", async () => {
    orgSecretNamesHolder.value = ["OPENROUTER_API_KEY", "DATADOG_API_KEY"];
    const user = userEvent.setup();
    render(<SessionProfileEditor mode="create" />);
    fireEvent.change(screen.getByLabelText(/profile name/i), { target: { value: "Gateway" } });

    await user.click(screen.getByRole("button", { name: /advanced/i }));
    await user.click(screen.getByRole("button", { name: /add secret/i }));

    await user.click(screen.getByRole("combobox", { name: "" }));
    await user.click(await screen.findByText("OPENROUTER_API_KEY"));
    fireEvent.change(screen.getByLabelText(/env var name/i), {
      target: { value: "ANTHROPIC_AUTH_TOKEN" },
    });

    fireEvent.click(screen.getByRole("button", { name: /create profile/i }));
    await waitFor(() => expect(create).toHaveBeenCalled());
    expect(create.mock.calls[0][0].secrets).toMatchObject([
      { ref: "OPENROUTER_API_KEY", envVar: "ANTHROPIC_AUTH_TOKEN", mode: "broker" },
    ]);
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

  it("sends an empty designation when the reviewer toggle is untouched", async () => {
    render(<SessionProfileEditor mode="create" />);
    fireEvent.change(screen.getByLabelText(/profile name/i), { target: { value: "Plain Agent" } });
    fireEvent.click(screen.getByRole("button", { name: /create profile/i }));
    await waitFor(() => expect(create).toHaveBeenCalled());
    expect(create.mock.calls[0][0]).toMatchObject({ designation: "" });
  });

  it("maps the PR reviewer toggle to the reviewer designation", async () => {
    render(<SessionProfileEditor mode="create" />);
    fireEvent.change(screen.getByLabelText(/profile name/i), {
      target: { value: "Review Agent" },
    });
    fireEvent.click(screen.getByLabelText(/pr reviewer profile/i));
    fireEvent.click(screen.getByRole("button", { name: /create profile/i }));
    await waitFor(() => expect(create).toHaveBeenCalled());
    expect(create.mock.calls[0][0]).toMatchObject({ designation: "pr_reviewer" });
  });

  it("toggling a skill includes it in the payload (ADR 0055)", async () => {
    render(<SessionProfileEditor mode="create" />);
    fireEvent.change(screen.getByLabelText(/profile name/i), {
      target: { value: "Browser Agent" },
    });
    openAdvanced();
    fireEvent.click(screen.getByTestId("skill-browser"));
    fireEvent.click(screen.getByRole("button", { name: /create profile/i }));
    await waitFor(() => expect(create).toHaveBeenCalled());
    expect(create.mock.calls[0][0].skills).toEqual(["browser"]);
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

  it("opens the skill file chooser from the primary upload action", () => {
    render(<SessionProfileEditor mode="create" />);
    openAdvanced();
    const fileInput = screen.getByTestId("skill-upload-file") as HTMLInputElement;
    const openFileChooser = vi.spyOn(fileInput, "click");

    expect(screen.getByText(/archive with skill\.md at its root/i)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /choose skill file/i }));

    expect(openFileChooser).toHaveBeenCalledOnce();
    expect(uploadSkill).not.toHaveBeenCalled();
  });
});

// ADR 0109: the Google Cloud block grants per-connection operations.
describe("SessionProfileEditor (Google Cloud connections)", () => {
  const googleConnection = (overrides: Record<string, unknown> = {}) => ({
    id: "gcp-1",
    alias: "prod-observer",
    provider: "gcp",
    displayName: "Prod observer",
    enabled: true,
    testedAt: "2026-08-01T00:00:00Z",
    createdAt: "",
    updatedAt: "",
    googleCloud: {
      workloadIdentityProvider:
        "//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/engrams/providers/engrams-prod",
      serviceAccountEmail: "observer@customer.iam.gserviceaccount.com",
      endpoints: ["logging.googleapis.com"],
    },
    ...overrides,
  });

  /** The PowerSelector row that carries an operation's curated label. */
  const operationRow = (label: string) => {
    const row = screen.getByText(label).closest("div");
    if (!row) throw new Error(`no row for ${label}`);
    return within(row);
  };

  it("offers only the operations the connection's endpoints enable (web-M4)", () => {
    connectionsHolder.value = [googleConnection()];
    render(<SessionProfileEditor mode="create" />);
    expect(screen.getByText("Prod observer")).toBeTruthy();
    // logging.googleapis.com enables the Logging read + the generic Google
    // API call; it does not enable Compute or a GKE control-plane call.
    expect(screen.getByText("Read Cloud Logging entries")).toBeTruthy();
    expect(screen.getByText("Call configured Google APIs")).toBeTruthy();
    expect(screen.queryByText("Describe Compute Engine instances")).toBeNull();
    expect(screen.queryByText("Call the configured GKE API server")).toBeNull();
  });

  it("carries a toggled grant into the create payload", async () => {
    connectionsHolder.value = [googleConnection()];
    render(<SessionProfileEditor mode="create" />);
    fireEvent.change(screen.getByLabelText(/profile name/i), {
      target: { value: "Observer" },
    });
    fireEvent.click(
      operationRow("Read Cloud Logging entries").getByRole("button", { name: /grant read/i }),
    );
    fireEvent.click(screen.getByRole("button", { name: /create profile/i }));
    await waitFor(() => expect(create).toHaveBeenCalled());
    expect(create.mock.calls[0][0].integrationGrants).toEqual([
      { connectionId: "gcp-1", operation: "logging.entries.list", resourceConstraints: [] },
    ]);
  });

  // web-H1: a disabled connection must stay visible with working toggles.
  // Editing endpoints auto-disables the connection, and a hidden grant
  // blocked every unrelated save of the profile with no way to remove it.
  it("renders a disabled connection with a badge and lets the grant be removed", async () => {
    connectionsHolder.value = [googleConnection({ enabled: false })];
    paramsHolder.value = { id: "p1" };
    profileHolder.value = {
      profile: {
        id: "p1",
        name: "Backend Agent",
        description: "",
        icon: "Bot",
        imageId: "i1",
        harness: "claude",
        model: "opus",
        effort: "high",
        includeUserTokens: false,
        envVars: {},
        integrationGrants: [
          { connectionId: "gcp-1", operation: "logging.entries.list", resourceConstraints: [] },
        ],
        skills: [],
        network: { default: "deny", allowHosts: [], allowHostPatterns: [] },
        secrets: [],
        portExposures: [],
      },
    };
    render(<SessionProfileEditor mode="edit" />);
    await screen.findByDisplayValue("Backend Agent");
    expect(screen.getByText("Disabled")).toBeTruthy();
    const pill = operationRow("Read Cloud Logging entries").getByRole("button", {
      name: /granted read/i,
    });
    expect(pill.getAttribute("aria-pressed")).toBe("true");
    fireEvent.click(pill);
    fireEvent.click(screen.getByRole("button", { name: /save changes/i }));
    await waitFor(() => expect(update).toHaveBeenCalledOnce());
    expect(update.mock.calls[0][0].integrationGrants).toEqual([]);
  });

  // web-M4 companion: a grant whose endpoint was removed from the connection
  // must stay visible (flagged) so it can be revoked.
  it("keeps an orphaned grant visible and removable after an endpoint edit", async () => {
    connectionsHolder.value = [googleConnection()];
    paramsHolder.value = { id: "p1" };
    profileHolder.value = {
      profile: {
        id: "p1",
        name: "Backend Agent",
        description: "",
        icon: "Bot",
        imageId: "i1",
        harness: "claude",
        model: "opus",
        effort: "high",
        includeUserTokens: false,
        envVars: {},
        integrationGrants: [
          { connectionId: "gcp-1", operation: "compute.instances.get", resourceConstraints: [] },
        ],
        skills: [],
        network: { default: "deny", allowHosts: [], allowHostPatterns: [] },
        secrets: [],
        portExposures: [],
      },
    };
    render(<SessionProfileEditor mode="edit" />);
    await screen.findByDisplayValue("Backend Agent");
    expect(screen.getByText(/not in this connection's allowed APIs/i)).toBeTruthy();
    const pill = operationRow("Describe Compute Engine instances").getByRole("button", {
      name: /granted read/i,
    });
    expect(pill.getAttribute("aria-pressed")).toBe("true");
    fireEvent.click(pill);
    fireEvent.click(screen.getByRole("button", { name: /save changes/i }));
    await waitFor(() => expect(update).toHaveBeenCalledOnce());
    expect(update.mock.calls[0][0].integrationGrants).toEqual([]);
  });
});

describe("SessionProfileEditor (edit)", () => {
  it("round-trips every existing profile field on an unchanged save", async () => {
    paramsHolder.value = { id: "p1" };
    profileHolder.value = {
      profile: {
        id: "p1",
        name: "Backend Agent",
        description: "Node API",
        icon: "Server",
        imageId: "i1",
        harness: "claude",
        model: "opus",
        effort: "high",
        designation: "pr_reviewer",
        includeUserTokens: true,
        envVars: { ANTHROPIC_MODEL: "claude-x" },
        integrationGrants: [
          {
            connectionId: "connection-github",
            operation: "issues:read",
            resourceConstraints: [],
          },
        ],
        skills: ["browser"],
        network: {
          default: "allow",
          allowHosts: ["db.internal"],
          allowHostPatterns: ["*.githubusercontent.com"],
        },
        secrets: [
          {
            ref: "db-password",
            envVar: "DB_PASSWORD",
            mode: "broker",
            allowHosts: ["db.internal"],
            allowHostPatterns: ["*.db.internal"],
          },
        ],
        portExposures: [3000, 8080],
      },
    };
    render(<SessionProfileEditor mode="edit" />);
    const nameInput = await screen.findByLabelText(/profile name/i);
    await waitFor(() => expect((nameInput as HTMLInputElement).value).toBe("Backend Agent"));
    await waitFor(() => {
      expect(screen.getByTestId("harness-select").textContent).toContain("Claude Code");
      expect(screen.getByTestId("model-select").textContent).toContain("Claude Opus 4.8");
      expect(screen.getByTestId("effort-select").textContent).toContain("High");
    });
    expect(screen.getByTestId("icon-picker").textContent).toContain("Server");
    expect(screen.getByTestId("image-select").textContent).toContain("registry/api:latest");
    expect(screen.getByLabelText(/pr reviewer profile/i).getAttribute("data-state")).toBe(
      "checked",
    );
    expect((screen.getByLabelText(/allowed hosts/i) as HTMLTextAreaElement).value).toBe(
      "db.internal",
    );
    expect((screen.getByLabelText(/host patterns/i) as HTMLTextAreaElement).value).toBe(
      "*.githubusercontent.com",
    );
    // env vars live under Advanced; opening it reveals the hydrated KEY input.
    fireEvent.click(screen.getByRole("button", { name: /advanced/i }));
    expect((screen.getByDisplayValue("ANTHROPIC_MODEL") as HTMLInputElement).value).toBe(
      "ANTHROPIC_MODEL",
    );
    expect(screen.getByTestId("skill-browser").getAttribute("data-state")).toBe("checked");
    expect(screen.getByTestId("port-chip-3000")).toBeTruthy();
    expect(screen.getByTestId("port-chip-8080")).toBeTruthy();
    expect(screen.getByText("db-password")).toBeTruthy();
    expect(screen.getByDisplayValue("DB_PASSWORD")).toBeTruthy();
    expect(
      screen
        .getByLabelText(/include the launching user's other saved tokens/i)
        .getAttribute("data-state"),
    ).toBe("checked");
    fireEvent.click(screen.getByRole("button", { name: /save changes/i }));
    await waitFor(() => expect(update).toHaveBeenCalledOnce());
    expect(update.mock.calls[0][0]).toMatchObject({
      id: "p1",
      name: "Backend Agent",
      description: "Node API",
      icon: "Server",
      imageId: "i1",
      harness: "claude",
      model: "opus",
      effort: "high",
      includeUserTokens: true,
      envVars: { ANTHROPIC_MODEL: "claude-x" },
      integrationGrants: [
        {
          connectionId: "connection-github",
          operation: "issues:read",
          resourceConstraints: [],
        },
      ],
      skills: ["browser"],
      network: {
        default: "allow",
        allowHosts: ["db.internal"],
        allowHostPatterns: ["*.githubusercontent.com"],
      },
      secrets: [
        {
          ref: "db-password",
          envVar: "DB_PASSWORD",
          mode: "broker",
          allowHosts: ["db.internal"],
          allowHostPatterns: ["*.db.internal"],
        },
      ],
      portExposures: [3000, 8080],
    });
    // An unchanged save must NOT re-send designation — otherwise a stale tab
    // could silently steal or drop the reviewer role on an unrelated edit.
    expect(update.mock.calls[0][0].designation).toBeUndefined();
  });

  it("sends designation only when the reviewer toggle is changed", async () => {
    paramsHolder.value = { id: "p1" };
    profileHolder.value = {
      profile: {
        id: "p1",
        name: "Backend Agent",
        description: "",
        icon: "Bot",
        imageId: "i1",
        harness: "claude",
        model: "opus",
        effort: "high",
        designation: "pr_reviewer",
        includeUserTokens: false,
        envVars: {},
        integrationGrants: [],
        skills: [],
        network: { default: "deny", allowHosts: [], allowHostPatterns: [] },
        secrets: [],
        portExposures: [],
      },
    };
    render(<SessionProfileEditor mode="edit" />);
    await screen.findByDisplayValue("Backend Agent");
    // Turn the reviewer toggle OFF, then save.
    fireEvent.click(screen.getByLabelText(/pr reviewer profile/i));
    fireEvent.click(screen.getByRole("button", { name: /save changes/i }));
    await waitFor(() => expect(update).toHaveBeenCalledOnce());
    // The toggle was flipped from on→off, so designation is sent as "" (clear).
    expect(update.mock.calls[0][0].designation).toBe("");
  });

  it("normalizes legacy blank model and effort values on save", async () => {
    paramsHolder.value = { id: "legacy" };
    profileHolder.value = {
      profile: {
        id: "legacy",
        name: "Legacy",
        description: "",
        icon: "Bot",
        imageId: "i1",
        harness: "claude",
        model: "",
        effort: "",
        includeUserTokens: false,
        envVars: {},
        integrationGrants: [],
        skills: [],
        network: { default: "deny", allowHosts: [], allowHostPatterns: [] },
        secrets: [],
        portExposures: [],
      },
    };
    render(<SessionProfileEditor mode="edit" />);
    await screen.findByDisplayValue("Legacy");
    fireEvent.click(screen.getByRole("button", { name: /save changes/i }));
    await waitFor(() => expect(update).toHaveBeenCalledOnce());
    expect(update.mock.calls[0][0]).toMatchObject({ id: "legacy", harness: "claude" });
    expect(update.mock.calls[0][0].model).toBeUndefined();
    expect(update.mock.calls[0][0].effort).toBeUndefined();
  });
});
