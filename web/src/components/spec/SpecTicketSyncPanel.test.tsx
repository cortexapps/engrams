import { screen } from "@testing-library/react";
import { describe, expect, test, vi } from "vitest";

import type { SpecTicket, SpecTicketSyncLedger } from "@/hooks/useSpecTicketSync";
import { renderWithProviders } from "@/test-utils";

const hooks = vi.hoisted(() => ({
  useSpecTickets: vi.fn(),
  useSpecTicketSyncLedger: vi.fn(),
  useSyncSpecTickets: vi.fn(),
}));
const { useSpecTickets, useSpecTicketSyncLedger, useSyncSpecTickets } = hooks;

vi.mock("@/hooks/useSpecTicketSync", () => hooks);

import { SpecTicketSyncPanel } from "./SpecTicketSyncPanel";

const ticket: SpecTicket = {
  id: "ticket-1",
  parentId: null,
  ordinal: 0,
  depth: 0,
  title: "Add the render cache",
  body: "",
  description: "",
  backlink: { sectionId: "sec-1", sectionTitle: "Problem", href: "/specs/spec-1#sec-1" },
  dependsOn: [],
  syncState: "draft",
  linearId: null,
  syncError: null,
  openQuestions: [],
};

function ledger(teamId: string | null): SpecTicketSyncLedger {
  return {
    specId: "spec-1",
    connector: { provider: "linear", connected: true, reason: null },
    target: {
      teamId,
      teamName: teamId ? "Platform" : null,
      projectId: null,
      projectName: null,
      labelIds: [],
      labelNames: [],
    },
    rows: [],
    overridden: false,
    total: 1,
    synced: 0,
    failed: 0,
    inFlight: 0,
  };
}

function mount(teamId: string | null) {
  useSpecTickets.mockReturnValue({
    data: { tickets: [ticket], docSeq: 62 },
    isPending: false,
    refetch: vi.fn(),
  });
  useSpecTicketSyncLedger.mockReturnValue({ data: ledger(teamId) });
  useSyncSpecTickets.mockReturnValue({ mutate: vi.fn(), isPending: false });
  renderWithProviders(<SpecTicketSyncPanel specId="spec-1" onBack={() => {}} />);
}

describe("SpecTicketSyncPanel", () => {
  // Syncing with no team fails server-side with `no_target`, and nothing on
  // this panel can choose one. An enabled button here is a click into an
  // error the reader cannot act on.
  test("will not offer a sync it cannot complete without a team", async () => {
    mount(null);

    const blocked = await screen.findByRole("button", { name: "Sync all to Linear" });
    expect((blocked as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByText(/Choose a Linear team/)).toBeTruthy();
  });

  test("offers the sync once a team is resolved", async () => {
    mount("team-platform");

    const offered = await screen.findByRole("button", { name: "Sync all to Linear" });
    expect((offered as HTMLButtonElement).disabled).toBe(false);
    expect(screen.queryByText(/Choose a Linear team/)).toBeNull();
  });
});
