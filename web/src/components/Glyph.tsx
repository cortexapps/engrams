import { motion } from "framer-motion";
import type { SessionState } from "../types";

// Status glyphs in the margin — these stand in for colored dots. The
// shape carries the meaning, not the color.
//
//   ●  active            ◐  created / guest_ready (starting)
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
  status: SessionState;
  /** Override beat (e.g. event ticker doesn't pulse). */
  beat?: boolean;
}

export function StatusGlyph({ status, beat = true }: GlyphProps) {
  const glyph = glyphFor(status);
  const tone = toneFor(status);
  const isLive = beat && status === "active";

  return (
    <motion.span
      key={glyph} // remount on change → triggers cross-fade
      initial={{ opacity: 0 }}
      animate={{ opacity: 1 }}
      exit={{ opacity: 0 }}
      transition={{ duration: 0.35, ease: "easeOut" }}
      className={`inline-block leading-none ${isLive ? "animate-pulse motion-reduce:animate-none" : ""}`}
      style={{ color: tone }}
      aria-label={status}
    >
      {glyph}
    </motion.span>
  );
}

function glyphFor(status: SessionState): string {
  switch (status) {
    case "pending":
    case "queued":
      return "○";
    case "created":
    case "guest_ready":
      return "◐";
    case "active":
      return "●";
    case "idle":
      return "◌";
    case "evicting":
    case "evacuating":
      return "◑";
    case "host_lost":
      return "⚠";
    case "completed":
      return "✓";
    case "failed":
      return "!";
    case "dead":
      return "✕";
  }
}

function toneFor(status: SessionState): string {
  switch (status) {
    case "active":
      return "var(--ring)"; // racing green on paper, lime on the dark ground
    case "idle":
    case "pending":
    case "queued":
    case "created":
    case "guest_ready":
    // Transitional suspend/relocate: faded like idle — on their way there
    // (or back to active), not in trouble.
    case "evicting":
    case "evacuating":
    case "completed":
    case "dead":
      return "var(--muted-foreground)";
    case "host_lost":
    case "failed":
      return "var(--destructive)";
  }
}
