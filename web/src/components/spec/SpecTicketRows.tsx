import type { SpecTicket, SpecTicketLinearIssue } from "@/hooks/useSpecTicketSync";

export interface SpecTicketRowsProps {
  tickets: SpecTicket[];
  /** The Linear identity of each synced ticket, from the ledger. */
  issues: Map<string, SpecTicketLinearIssue>;
}

/**
 * The ticket tree with its sync state (mock 2l).
 *
 * A failed ticket keeps its place in the tree with an instrument-red pill. It
 * is not moved to the bottom, not hidden behind a filter, and not replaced by
 * an error card: the tree is the plan, and one row that did not land does not
 * change the plan (R41).
 */
export function SpecTicketRows({ tickets, issues }: SpecTicketRowsProps) {
  return (
    <ul className="spec-ticket-rows">
      {tickets.map((ticket) => {
        const issue = issues.get(ticket.id) ?? null;
        return (
          <li
            key={ticket.id}
            className={`spec-ticket-row spec-ticket-row-${ticket.syncState}`}
            style={{ marginInlineStart: `${Math.min(ticket.depth, 4) * 1.6}rem` }}
          >
            <span className={`spec-ticket-dot spec-ticket-dot-${ticket.syncState}`} aria-hidden />
            <span className="spec-ticket-title">{ticket.title}</span>
            {ticket.openQuestions.length > 0 ? (
              <span
                className="spec-ticket-questions"
                title={ticket.openQuestions.map((question) => question.text).join("\n")}
              >
                ⚑{ticket.openQuestions.length}
              </span>
            ) : null}
            <span className="spec-ticket-backlink">§{ticket.backlink.sectionTitle}</span>
            <SyncState ticket={ticket} issue={issue} />
          </li>
        );
      })}
    </ul>
  );
}

/** The right-hand status of one row: an identity, a state, or a reason. */
function SyncState({ ticket, issue }: { ticket: SpecTicket; issue: SpecTicketLinearIssue | null }) {
  switch (ticket.syncState) {
    case "synced":
      // R43: after a ticket syncs, later edits happen in Linear, so the row
      // stops being a draft and becomes a link out.
      return issue ? (
        <a
          className="spec-ticket-identity"
          href={issue.url}
          target="_blank"
          rel="noreferrer"
          aria-label={`${ticket.title} in Linear: ${issue.identifier}`}
        >
          {issue.identifier} ↗
        </a>
      ) : (
        <span className="spec-ticket-identity">synced</span>
      );
    case "failed":
      return (
        <span className="spec-ticket-pill spec-ticket-pill-failed" title={ticket.syncError ?? ""}>
          failed{shortReason(ticket.syncError)}
        </span>
      );
    case "syncing":
      return <span className="spec-ticket-state">syncing…</span>;
    case "queued":
      return <span className="spec-ticket-state">queued</span>;
    default:
      return <span className="spec-ticket-state">draft</span>;
  }
}

/**
 * The pill has room for a status code, not a sentence. The whole reason is one
 * hover away, and it is spelled out in the rail below.
 */
function shortReason(error: string | null): string {
  if (!error) return "";
  const status = /\b(4\d\d|5\d\d)\b/.exec(error);
  return status ? ` · ${status[1]}` : "";
}
