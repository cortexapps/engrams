import { describe, it, expect, vi, beforeEach } from "vitest";
import { screen, fireEvent, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "../test-utils";
import { NewSessionDialog } from "./NewSessionDialog";

type Profile = {
  id: string;
  name: string;
  description: string;
  icon: string;
  imageId: string;
  includeUserTokens: boolean;
  envVars: Record<string, string>;
  archived: boolean;
};

const TWO_PROFILES: Profile[] = [
  {
    id: "p1",
    name: "Backend Agent",
    description: "Node API",
    icon: "Server",
    imageId: "i1",
    includeUserTokens: true,
    envVars: {},
    archived: false,
  },
  {
    id: "p2",
    name: "Frontend Agent",
    description: "React app",
    icon: "Code",
    imageId: "i2",
    includeUserTokens: false,
    envVars: {},
    archived: false,
  },
];

// Use vi.hoisted so the mutable holder is set up before the factory runs
// (vi.mock is hoisted to the top of the file, before imports).
const mockProfilesData = vi.hoisted(() => ({
  profiles: [] as unknown[],
  isPending: false,
  error: null as unknown,
  refetch: vi.fn(),
}));

vi.mock("../hooks/useProfiles", () => ({
  useProfiles: () => ({
    data: { profiles: mockProfilesData.profiles },
    isPending: mockProfilesData.isPending,
    error: mockProfilesData.error,
    refetch: mockProfilesData.refetch,
  }),
}));

beforeEach(() => {
  // Reset to the default two-profile state before each test
  mockProfilesData.profiles = TWO_PROFILES;
  mockProfilesData.isPending = false;
  mockProfilesData.error = null;
  mockProfilesData.refetch.mockClear();
});

describe("NewSessionDialog (profile picker)", () => {
  it("lists profiles and filters by the search box", async () => {
    renderWithProviders(
      <NewSessionDialog open onOpenChange={() => {}} showTrigger={false} onCreated={() => {}} />,
    );
    // Router defers the initial render to a microtask — await the first match.
    expect(await screen.findByText("Backend Agent")).toBeTruthy();
    expect(screen.getByText("Frontend Agent")).toBeTruthy();
    fireEvent.change(screen.getByPlaceholderText("Search profiles…"), {
      target: { value: "front" },
    });
    await waitFor(() => expect(screen.queryByText("Backend Agent")).toBeNull());
    expect(screen.getByText("Frontend Agent")).toBeTruthy();
  });

  it("shows the task field always (every profile is an agent session)", async () => {
    renderWithProviders(
      <NewSessionDialog open onOpenChange={() => {}} showTrigger={false} onCreated={() => {}} />,
    );
    // Router defers the initial render to a microtask — await the first match.
    expect(await screen.findByPlaceholderText("Describe the task…")).toBeTruthy();
  });

  it("shows empty state when no profiles are configured", async () => {
    mockProfilesData.profiles = [];
    renderWithProviders(
      <NewSessionDialog open onOpenChange={() => {}} showTrigger={false} onCreated={() => {}} />,
    );
    // Router defers the initial render to a microtask — await the first match.
    expect(
      await screen.findByText("No profiles configured — contact an admin to set one up."),
    ).toBeTruthy();
  });

  it("keeps Start disabled until a profile is chosen, then reinforces the credential risk", async () => {
    const user = userEvent.setup();
    renderWithProviders(
      <NewSessionDialog open onOpenChange={() => {}} showTrigger={false} onCreated={() => {}} />,
    );
    const start = (await screen.findByTestId("start-session")) as HTMLButtonElement;
    expect(start.disabled).toBe(true);

    // Choosing the token-carrying profile enables Start and surfaces the
    // launch-time credential note (the high-stakes reinforcement).
    await user.click(screen.getByTestId("profile-row-p1"));
    await waitFor(() => expect(start.disabled).toBe(false));
    expect(
      screen.getByText("This profile carries your Claude token into the sandbox."),
    ).toBeTruthy();
  });

  it("selects the highlighted profile by keyboard (cmdk: type, ↓, Enter)", async () => {
    const user = userEvent.setup();
    renderWithProviders(
      <NewSessionDialog open onOpenChange={() => {}} showTrigger={false} onCreated={() => {}} />,
    );
    const search = await screen.findByPlaceholderText("Search profiles…");
    search.focus();
    // Filter to the non-token profile, then take it via the keyboard.
    await user.keyboard("front");
    await user.keyboard("{Enter}");
    const start = screen.getByTestId("start-session") as HTMLButtonElement;
    await waitFor(() => expect(start.disabled).toBe(false));
    // p2 doesn't carry tokens — no launch-time note.
    expect(
      screen.queryByText("This profile carries your Claude token into the sandbox."),
    ).toBeNull();
  });

  it("surfaces a load error with a retry instead of the empty state", async () => {
    mockProfilesData.error = new Error("network down");
    const user = userEvent.setup();
    renderWithProviders(
      <NewSessionDialog open onOpenChange={() => {}} showTrigger={false} onCreated={() => {}} />,
    );
    expect(await screen.findByText("Couldn’t load profiles.")).toBeTruthy();
    // A load failure must not masquerade as "no profiles configured".
    expect(
      screen.queryByText("No profiles configured — contact an admin to set one up."),
    ).toBeNull();
    await user.click(screen.getByRole("button", { name: /retry/i }));
    expect(mockProfilesData.refetch).toHaveBeenCalled();
  });
});
