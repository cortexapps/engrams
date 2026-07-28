import { motion, useReducedMotion } from "framer-motion";

import { stageOf } from "./review-format";
import { cn } from "@/lib/utils";

/**
 * A review pass's stage, in the margin. Mirrors `StatusGlyph`: the shape carries
 * the meaning, colour is secondary, and callers always render the word beside it —
 * use `ReviewStage` unless you are supplying the word yourself. Crossfades on
 * change so a pass advancing from finding to verifying reads as movement rather
 * than a flicker.
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
  // `motion-reduce:` is a CSS variant and cannot reach a JS-driven animation, and
  // framer has no global reduced-motion honour in this app, so the hook is the
  // only thing standing between this and an unguarded fade.
  const still = useReducedMotion();

  return (
    <motion.span
      key={stage.glyph}
      initial={still ? false : { opacity: 0 }}
      animate={{ opacity: 1 }}
      transition={{ duration: still ? 0 : 0.35, ease: "easeOut" }}
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

/**
 * Glyph plus word — the pairing the accessibility rule requires, and the only
 * status form that should reach a content surface.
 *
 * The word is INK, not the instrument tone. `index.css` states the constraint the
 * tokens were chosen against: "status text always pairs a colored dot with ink,
 * never colored body copy, so AA holds." Amber-on-paper measures 2.7:1 as text,
 * which is below even the large-text floor; as a glyph beside an ink word it is
 * redundant decoration and the pairing holds up.
 */
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
      <span>{stage.label}</span>
    </span>
  );
}
