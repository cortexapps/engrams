import { render, screen } from "@testing-library/react";
import { describe, expect, test } from "vitest";

import { SpecTicketSyncBadge } from "./SpecTicketSyncBadge";

describe("SpecTicketSyncBadge", () => {
  test("uses the nominal grammar only when all tickets are synced", () => {
    const { rerender } = render(<SpecTicketSyncBadge state="pending" />);
    expect(screen.getByText("Syncing").className).not.toContain("instrument-nominal");

    rerender(<SpecTicketSyncBadge state="synced" />);
    const synced = screen.getByText("Synced");
    expect(synced.dataset.state).toBe("synced");
    expect(synced.className).toContain("text-instrument-nominal-ink");
  });

  test("surfaces a failed ticket row", () => {
    render(<SpecTicketSyncBadge state="failed" />);
    expect(screen.getByText("Sync failed").dataset.state).toBe("failed");
  });

  test("omits the badge before a ticket draft exists", () => {
    const { container } = render(<SpecTicketSyncBadge state="none" />);
    expect(container.childElementCount).toBe(0);
  });
});
