import { fmtBytes, hms } from "./transcriptFmt";

// Durability rhythm — the Engrams signature, currently invisible in the
// transcript (ADR 0030 §2c). A faint, centered verdigris marker that
// ties the conversation to the snapshot / sleep / resume lifecycle:
//
//   ⌑ snapshotted · 1.2 GiB
//   ⌑ resumed from snapshot
//
// Rendered from the `snapshot_taken` / `resumed` events.

export function DurabilityMarker({
  mark,
  sizeBytes,
  at,
}: {
  mark: "snapshot" | "resumed";
  sizeBytes?: number;
  at?: string;
}) {
  const label =
    mark === "snapshot"
      ? `snapshotted${sizeBytes != null ? ` · ${fmtBytes(sizeBytes)}` : ""}`
      : "resumed from snapshot";
  return (
    <div className="durability-marker">
      <span aria-hidden style={{ color: "var(--accent-archived)", fontSize: "0.85rem" }}>
        ⌑
      </span>
      <span className="durability-label section-label">{label}</span>
      {at && <span className="durability-time font-mono">{hms(at)}</span>}
    </div>
  );
}
