import { Link } from "@tanstack/react-router";

import { Button } from "@/components/ui/button";
import type { SpecTicketSyncLedger } from "@/hooks/useSpecTicketSync";

export interface SpecTicketSyncLedgerRailProps {
  ledger: SpecTicketSyncLedger;
  /** The row a request is in flight for, so its button reads as busy. */
  pendingTicketId: string | null;
  onRetry: (ticketId: string) => void;
}

/**
 * The sync rail (mock 2l).
 *
 * Failure is a first-class row here, not a toast: every failed ticket keeps a
 * block of its own that says what Linear answered and offers the two honest
 * actions. Retry is idempotent (N4), so pressing it twice is safe — and the
 * copy says so rather than hiding the button behind a confirmation.
 */
export function SpecTicketSyncLedgerRail({
  ledger,
  pendingTicketId,
  onRetry,
}: SpecTicketSyncLedgerRailProps) {
  const failures = ledger.rows.filter((row) => row.syncState === "failed");
  const progress = ledger.total === 0 ? 0 : Math.round((ledger.synced / ledger.total) * 100);

  return (
    <aside className="spec-ticket-rail" aria-label="Linear sync">
      <div className="spec-ticket-rail-head">
        <span className="spec-ticket-rail-title">Linear sync</span>
        <span className="spec-ticket-rail-count">
          {ledger.synced} / {ledger.total}
        </span>
      </div>
      <div className="spec-ticket-progress" role="presentation">
        <div className="spec-ticket-progress-fill" style={{ width: `${progress}%` }} />
      </div>

      {ledger.connector.connected ? (
        <dl className="spec-ticket-target">
          <div>
            <dt>team</dt>
            <dd>{ledger.target.teamName ?? ledger.target.teamId ?? "not chosen"}</dd>
          </div>
          <div>
            <dt>project</dt>
            <dd>{ledger.target.projectName ?? ledger.target.projectId ?? "none"}</dd>
          </div>
          <div>
            <dt>labels</dt>
            <dd>
              {ledger.target.labelNames.length > 0 ? ledger.target.labelNames.join(", ") : "none"}
            </dd>
          </div>
          {ledger.overridden ? (
            <div>
              <dt>scope</dt>
              <dd>this spec only</dd>
            </div>
          ) : null}
        </dl>
      ) : (
        <div className="spec-ticket-disconnected">
          <p>{ledger.connector.reason ?? "Linear is not connected."}</p>
          <p className="spec-ticket-rail-note">
            The tree stays fully editable. Connect Linear when you want the tickets to land.
          </p>
          <Button asChild size="sm">
            <Link to="/settings/integrations">Connect Linear</Link>
          </Button>
        </div>
      )}

      {failures.map((row) => (
        <div className="spec-ticket-failure" key={row.ticketId}>
          <span className="spec-ticket-failure-title">sync failed · {row.title}</span>
          <p>{row.error ?? "Linear refused this ticket."}</p>
          <p className="spec-ticket-rail-note">
            The other rows were not blocked. Retry is safe to press twice — it never creates a
            second issue.
          </p>
          <div className="spec-ticket-failure-actions">
            <Button asChild size="sm" variant="outline">
              <Link to="/settings/integrations">Reconnect Linear</Link>
            </Button>
            <Button
              size="sm"
              variant="outline"
              disabled={pendingTicketId === row.ticketId}
              onClick={() => onRetry(row.ticketId)}
            >
              {pendingTicketId === row.ticketId ? "Retrying…" : "Retry row"}
            </Button>
          </div>
        </div>
      ))}

      {ledger.inFlight > 0 ? (
        <p className="spec-ticket-rail-note">
          {ledger.inFlight} row{ledger.inFlight === 1 ? "" : "s"} in flight. The batch is durable:
          it finishes even if you close this tab.
        </p>
      ) : null}
    </aside>
  );
}
