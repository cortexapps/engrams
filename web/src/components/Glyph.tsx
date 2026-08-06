import { motion } from "framer-motion";
import type { ListRowState } from "../lib/types";

// Status glyphs in the margin — these stand in for colored dots. The
// shape carries the meaning, not the color.
//
//   ●  active            ◐  created (starting)
//   ◌  idle              ○  pending
//   ⚠  host_lost         ✓  completed
//   ✕  dead              !  failed
//   ◑  evicting / evacuating (transitional: suspend/relocate in
//      flight — the half-moon mirrors the starting states' ◐)
//
// Active sessions get a slow opacity heartbeat. Active renders in the
// theme ring (racing green on paper, lime on the dark ground) to read as
// live; idle/booting/done fade into muted ink as archival; host_lost and
// failed render destructive to flag that they need attention (snapshot
// exists → /resume; no snapshot → going Dead shortly).

export interface GlyphProps {
  status: ListRowState;
  /** Override beat (e.g. event ticker doesn't pulse). */
  beat?: boolean;
  /** ADR 0107: the session is waiting on the USER (a plan awaiting review,
   * an unanswered question). Keeps the lifecycle shape but re-tones amber
   * and joins the heartbeat — impossible to miss on a muted rail. */
  attention?: boolean;
}

export function StatusGlyph({ status, beat = true, attention = false }: GlyphProps) {
  const glyph = glyphFor(status);
  const tone = attention ? "var(--instrument-caution)" : toneFor(status);
  const isLive = beat && (status === "active" || attention);

  return (
    <motion.span
      key={glyph} // remount on change → triggers cross-fade
      initial={{ opacity: 0 }}
      animate={{ opacity: 1 }}
      exit={{ opacity: 0 }}
      transition={{ duration: 0.35, ease: "easeOut" }}
      className={`inline-block leading-none ${isLive ? "animate-pulse motion-reduce:animate-none" : ""}`}
      style={{ color: tone }}
      aria-label={attention ? `${status} — waiting on you` : status}
    >
      {glyph}
    </motion.span>
  );
}

function glyphFor(status: ListRowState): string {
  switch (status) {
    case "pending":
    case "queued":
      return "○";
    case "created":
      return "◐";
    case "active":
      return "●";
    case "parked":
      // Paused in place: alive (filled) but held — distinct from both
      // the running ● and the snapshotted ◌.
      return "◉";
    case "idle":
      return "◌";
    case "evicting":
    case "evacuating":
      return "◑";
    case "unreachable":
    case "host_lost":
      return "⚠";
    case "completed":
      return "✓";
    case "failed":
      return "!";
    case "dead":
      return "✕";
    // We asked and nobody answered. A hollow square is not on the lifecycle
    // ramp (○ ◐ ● ◌ ✓ ✕), so it cannot be misread as a position on it.
    case "unknown":
      return "▫";
  }
}

function toneFor(status: ListRowState): string {
  switch (status) {
    case "active":
      return "var(--ring)"; // racing green on paper, lime on the dark ground
    case "idle":
    case "pending":
    case "queued":
    case "created":
    // Parked: resting with a live VM — faded like idle (cheap to wake,
    // not in trouble).
    case "parked":
    // Transitional suspend/relocate: faded like idle — on their way there
    // (or back to active), not in trouble.
    case "evicting":
    case "evacuating":
    case "completed":
    case "dead":
    // Not a fault — we simply do not know. Quiet, never destructive.
    case "unknown":
      return "var(--muted-foreground)";
    case "unreachable":
    case "host_lost":
    case "failed":
      return "var(--destructive)";
  }
}
