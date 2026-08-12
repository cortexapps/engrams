import { useState } from "react";

import { Button } from "@/components/ui/button";
import {
  useSpecTickets,
  useSpecTicketSyncLedger,
  useSyncSpecTickets,
  type SpecTicketLinearIssue,
} from "@/hooks/useSpecTicketSync";

import { SpecTicketRows } from "./SpecTicketRows";
import { SpecTicketSyncLedgerRail } from "./SpecTicketSyncLedger";
import "./spec-ticket-sync.css";

export interface SpecTicketSyncPanelProps {
  specId: string;
  onBack: () => void;
}

/**
 * Ticketize (mock 2l): the tree on the canvas, the sync ledger on the rail.
 *
 * The two halves read the same rows from two routes — the tree carries the
 * plan, the ledger carries what Linear did with it — so a row that failed
 * keeps its place on the left while the reason and the two actions sit on the
 * right.
 */
export function SpecTicketSyncPanel({ specId, onBack }: SpecTicketSyncPanelProps) {
  const tree = useSpecTickets(specId);
  const ledger = useSpecTicketSyncLedger(specId);
  const sync = useSyncSpecTickets(specId);
  const [error, setError] = useState<string | null>(null);
  const [pendingTicketId, setPendingTicketId] = useState<string | null>(null);

  const tickets = tree.data?.tickets ?? [];
  const issues = new Map<string, SpecTicketLinearIssue>(
    (ledger.data?.rows ?? []).flatMap((row) => (row.issue ? [[row.ticketId, row.issue]] : [])),
  );
  const connected = ledger.data?.connector.connected ?? false;
  const pending = tickets.filter((ticket) => ticket.syncState !== "synced").length;

  const runSync = (ticketIds?: string[]) => {
    setError(null);
    setPendingTicketId(ticketIds?.[0] ?? null);
    sync.mutate(ticketIds === undefined ? {} : { ticketIds }, {
      onError: (cause) => setError(cause.message),
      onSettled: () => {
        setPendingTicketId(null);
        void tree.refetch();
      },
    });
  };

  return (
    <section className="spec-ticket-panel" aria-label="Tickets">
      <div className="spec-ticket-head">
        <span className="spec-ticket-panel-title">Tickets · {tickets.length}</span>
        {tree.data ? (
          <span className="spec-ticket-pinned">spec v{tree.data.docSeq} pinned</span>
        ) : null}
        <span className="spec-ticket-head-actions">
          <Button
            size="sm"
            disabled={!connected || pending === 0 || sync.isPending}
            onClick={() => runSync()}
          >
            {sync.isPending && pendingTicketId === null ? "Syncing…" : "Sync all to Linear"}
          </Button>
          <Button size="sm" variant="outline" onClick={onBack}>
            Back to spec
          </Button>
        </span>
      </div>

      {tree.isPending ? <p className="spec-ticket-empty">Loading the tree…</p> : null}
      {!tree.isPending && tickets.length === 0 ? (
        <p className="spec-ticket-empty">
          This spec has no proposed tickets yet. The agent proposes the first tree from the pinned
          spec once it is published.
        </p>
      ) : null}

      <div className="spec-ticket-layout">
        <SpecTicketRows tickets={tickets} issues={issues} />
        {ledger.data ? (
          <SpecTicketSyncLedgerRail
            ledger={ledger.data}
            pendingTicketId={pendingTicketId}
            onRetry={(ticketId) => runSync([ticketId])}
          />
        ) : null}
      </div>

      {error ? <p className="spec-action-error">{error}</p> : null}
    </section>
  );
}
