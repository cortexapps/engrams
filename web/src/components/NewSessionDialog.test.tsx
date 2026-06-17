import { describe, it, expect, vi, beforeEach } from "vitest";
import { screen, fireEvent, waitFor } from "@testing-library/react";
import { renderWithProviders } from "../test-utils";
import { NewSessionDialog } from "./NewSessionDialog";

// Use vi.hoisted so the mutable holder is set up before the factory runs
// (vi.mock is hoisted to the top of the file, before imports).
const mockProfilesData = vi.hoisted(() => ({
  profiles: [
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
  ] as Array<{
    id: string;
    name: string;
    description: string;
    icon: string;
    imageId: string;
    includeUserTokens: boolean;
    envVars: Record<string, string>;
    archived: boolean;
  }>,
  isPending: false,
}));

vi.mock("../hooks/useProfiles", () => ({
  useProfiles: () => ({
    data: { profiles: mockProfilesData.profiles },
    isPending: mockProfilesData.isPending,
  }),
}));

beforeEach(() => {
  // Reset to the default two-profile state before each test
  mockProfilesData.profiles = [
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
  mockProfilesData.isPending = false;
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
    expect(await screen.findByPlaceholderText("Describe the task for this session…")).toBeTruthy();
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
});
