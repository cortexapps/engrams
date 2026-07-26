import { motion } from "framer-motion";

import { stageOf } from "./review-format";
import { cn } from "@/lib/utils";

/**
 * A review pass's stage, in the margin. Mirrors `StatusGlyph`: the shape carries
 * the meaning, colour is secondary, and callers always render the word beside
 * it. Crossfades on change so a pass advancing from finding to verifying reads
 * as movement rather than a flicker.
 */
export function ReviewGlyph({
  status,
  beat = true,
  className,
}: {
  status: string;
  /** Suppress the working heartbeat (e.g. in a dense log line). */
  beat?: boolean;
  className?: string;
}) {
  const stage = stageOf(status);
  return (
    <motion.span
      key={stage.glyph}
      initial={{ opacity: 0 }}
      animate={{ opacity: 1 }}
      transition={{ duration: 0.35, ease: "easeOut" }}
      className={cn(
        "inline-block leading-none",
        beat && stage.live && "animate-pulse motion-reduce:animate-none",
        className,
      )}
      style={{ color: stage.tone }}
      aria-hidden
    >
      {stage.glyph}
    </motion.span>
  );
}

/** Glyph plus word — the pairing the accessibility rule requires. */
export function ReviewStage({
  status,
  className,
  beat = true,
}: {
  status: string;
  className?: string;
  beat?: boolean;
}) {
  const stage = stageOf(status);
  return (
    <span className={cn("inline-flex items-center gap-1.5", className)}>
      <ReviewGlyph status={status} beat={beat} />
      <span style={{ color: stage.tone }}>{stage.label}</span>
    </span>
  );
}
