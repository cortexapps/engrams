import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, test, vi } from "vitest";

import type {
  SpecTicket,
  SpecTicketLinearIssue,
  SpecTicketSyncLedger,
} from "@/hooks/useSpecTicketSync";
import { renderWithProviders } from "@/test-utils";

import { SpecTicketRows } from "./SpecTicketRows";
import { SpecTicketSyncLedgerRail } from "./SpecTicketSyncLedger";

function ticket(overrides: Partial<SpecTicket> & Pick<SpecTicket, "id">): SpecTicket {
  return {
    parentId: null,
    ordinal: 0,
    depth: 0,
    title: "Add org quota columns + backfill",
    body: "Add the columns.",
    description: "Add the columns.",
    backlink: {
      sectionId: "sec-data",
      sectionTitle: "Data model",
      href: "/specs/spec-1#sec-data",
    },
    dependsOn: [],
    syncState: "draft",
    linearId: null,
    syncError: null,
    openQuestions: [],
    ...overrides,
  };
}

function ledger(overrides: Partial<SpecTicketSyncLedger> = {}): SpecTicketSyncLedger {
  return {
    specId: "spec-1",
    connector: { provider: "linear", connected: true, reason: null },
    target: {
      teamId: "team-platform",
      teamName: "Platform",
      projectId: "project-quota",
      projectName: "Quota & billing",
      labelIds: ["l1", "l2"],
      labelNames: ["spec-mode", "quota"],
    },
    overridden: false,
    total: 6,
    synced: 2,
    failed: 1,
    inFlight: 1,
    rows: [
      {
        ticketId: "t4",
        title: "Enforce org quota in the gateway limiter",
        syncState: "failed",
        issue: null,
        error: "Linear returned 401 for create issue: the token expired.",
      },
    ],
    ...overrides,
  };
}

describe("SpecTicketRows", () => {
  test("a synced row shows its Linear identity and links out", () => {
    const issue: SpecTicketLinearIssue = {
      id: "issue-1",
      identifier: "ENG-412",
      url: "https://linear.app/acme/issue/ENG-412",
    };
    render(
      <SpecTicketRows
        tickets={[ticket({ id: "t1", syncState: "synced", linearId: "issue-1" })]}
        issues={new Map([["t1", issue]])}
      />,
    );

    // R43: after a ticket syncs, the row is a link into Linear, not a draft.
    const link = screen.getByRole("link", {
      name: "Add org quota columns + backfill in Linear: ENG-412",
    });
    expect(link.getAttribute("href")).toBe("https://linear.app/acme/issue/ENG-412");
  });

  test("a failed row keeps its place, with the status on the pill", () => {
    render(
      <SpecTicketRows
        tickets={[
          ticket({ id: "t1", title: "First", syncState: "synced" }),
          ticket({
            id: "t2",
            title: "Enforce org quota in the gateway limiter",
            syncState: "failed",
            syncError: "Linear returned 401 for create issue",
          }),
          ticket({ id: "t3", title: "Third", syncState: "draft" }),
        ]}
        issues={new Map()}
      />,
    );

    // R41: it is still the second of three rows, not moved or hidden.
    const rows = screen.getAllByRole("listitem");
    expect(rows).toHaveLength(3);
    expect(rows[1]?.textContent).toContain("Enforce org quota in the gateway limiter");
    expect(rows[1]?.textContent).toContain("failed · 401");
    expect(rows[1]?.className).toContain("spec-ticket-row-failed");
  });

  test("an in-flight row says so, and a draft says nothing more", () => {
    render(
      <SpecTicketRows
        tickets={[
          ticket({ id: "t1", syncState: "syncing" }),
          ticket({ id: "t2", syncState: "draft" }),
        ]}
        issues={new Map()}
      />,
    );

    expect(screen.getByText("syncing…")).not.toBeNull();
    expect(screen.getByText("draft")).not.toBeNull();
  });
});

describe("SpecTicketSyncLedgerRail", () => {
  test("the failure explains itself and offers reconnect and retry", async () => {
    const user = userEvent.setup();
    const onRetry = vi.fn();
    renderWithProviders(
      <SpecTicketSyncLedgerRail ledger={ledger()} pendingTicketId={null} onRetry={onRetry} />,
    );

    expect(await screen.findByText("2 / 6")).not.toBeNull();
    expect(screen.getByText(/401/)).not.toBeNull();
    expect(screen.getByText(/other rows were not blocked/)).not.toBeNull();
    expect(screen.getByRole("link", { name: "Reconnect Linear" })).not.toBeNull();

    await user.click(await screen.findByRole("button", { name: "Retry row" }));

    expect(onRetry).toHaveBeenCalledWith("t4");
  });

  test("the target is shown, and an override says it is this spec only", async () => {
    renderWithProviders(
      <SpecTicketSyncLedgerRail
        ledger={ledger({ overridden: true })}
        pendingTicketId={null}
        onRetry={vi.fn()}
      />,
    );

    expect(await screen.findByText("Platform")).not.toBeNull();
    expect(screen.getByText("Quota & billing")).not.toBeNull();
    expect(screen.getByText("spec-mode, quota")).not.toBeNull();
    // R45: the person can see that this choice does not change the org default.
    expect(screen.getByText("this spec only")).not.toBeNull();
  });

  test("no connector points at connecting, and never at an error", async () => {
    renderWithProviders(
      <SpecTicketSyncLedgerRail
        ledger={ledger({
          connector: {
            provider: "linear",
            connected: false,
            reason: "Linear is not connected yet. Connect it in Settings → Integrations.",
          },
          synced: 0,
          failed: 0,
          inFlight: 0,
          rows: [],
        })}
        pendingTicketId={null}
        onRetry={vi.fn()}
      />,
    );

    // R42: the call to action is to connect, and the tree is still editable.
    expect(await screen.findByRole("link", { name: "Connect Linear" })).not.toBeNull();
    expect(screen.getByText(/tree stays fully editable/)).not.toBeNull();
  });

  test("a retry in flight reads as busy", async () => {
    renderWithProviders(
      <SpecTicketSyncLedgerRail ledger={ledger()} pendingTicketId="t4" onRetry={vi.fn()} />,
    );

    const retry = await screen.findByRole("button", { name: "Retrying…" });
    expect((retry as HTMLButtonElement).disabled).toBe(true);
  });
});
