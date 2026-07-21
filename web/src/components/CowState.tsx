import { useSessionCowState } from "../hooks/useCowState";
import { useSession } from "../hooks/useSessions";
import { fmtAgo, fmtBytes } from "../format";
import { MetricRow } from "./MetricRow";

// ADR 0016 Phase A: per-session COW diagnostic.
//
// `<SessionCowState sessionId=… />` — a single session's durability view,
// rendered as a compact key/value readout in the session-detail rail.
// Always visible (it's the point of the page when a session is mid-action).
// Pulls from the 2s-polling hook in `hooks/useCowState.ts`.
//
// ADR 0029 moved the host-wide / fleet-wide COW view onto the dedicated
// Storage surface as a first-class durability ledger; only the per-session
// view lives here.

export function SessionCowState({ sessionId }: { sessionId: string }) {
  const query = useSessionCowState(sessionId);
  // ADR 0016 Phase B commit 8: the previous copy "no live sandbox
  // — disk-tier diagnostic unavailable while the session is idle /
  // lost / pending" lied about Active sessions whose host hasn't
  // wired the chunked-disk pipeline. Pulling session status here so
  // the copy can be honest about which case the user is in.
  const sessionQuery = useSession(sessionId);

  if (query.isPending) {
    return <p className="text-sm text-muted-foreground">loading COW state…</p>;
  }
  if (query.error) {
    return <p className="text-sm text-muted-foreground">{(query.error as Error).message}</p>;
  }

  const state = query.data?.state;
  if (!state) {
    // Conditional copy: terminal/idle states say so honestly;
    // Active-with-null means the host isn't running the chunked-disk
    // pipeline (no nbd_pool / chunk_store wired, or a pre-commit-7 host
    // that lost tracking on its last restart).
    const status = sessionQuery.data?.status;
    const message =
      status === undefined
        ? "disk-tier diagnostic unavailable."
        : status === "active"
          ? "no chunked-disk tracking for this Active task — the host hasn’t wired the NBD pipeline, or its tracking didn’t survive the last restart."
          : status === "parked"
            ? "task is parked (VM paused in place) — its disk tier is live; durability continues on the checkpoint cadence."
            : status === "idle" ||
                status === "host_lost" ||
                status === "evacuating" ||
                status === "evicting"
              ? `task is ${status.replace("_", " ")}; durability lives on the latest snapshot row.`
              : `task is ${status.replace("_", " ")} (terminal) — no live disk tier.`;
    return <p className="text-sm text-muted-foreground italic">{message}</p>;
  }

  const localPct =
    state.base_chunks > 0 ? Math.round((state.base_chunks_local / state.base_chunks) * 100) : null;

  return (
    <dl className="space-y-2 text-sm">
      <MetricRow label="dirty" value={`${state.dirty_chunks} chunks`} />
      <MetricRow label="size" value={fmtBytes(state.dirty_bytes)} />
      <MetricRow
        label="locality"
        value={localPct !== null ? `${localPct}% local` : "—"}
        title={`base chunks: ${state.base_chunks_local}/${state.base_chunks}`}
      />
      <MetricRow
        label="rpo"
        value={`flush ${fmtAgo(state.last_flush_at)}`}
        title={`last flush ${state.last_flush_at ?? "never"} · last snapshot ${state.last_snapshot_at ?? "never"}`}
      />
    </dl>
  );
}
