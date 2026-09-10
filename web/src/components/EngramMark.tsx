import { useEffect, useId, useRef } from "react";

// The engram trace — one silhouette, always. The base trace
// (0,3)→(1,2)→(2,3)→(3,1)→(4,2) on the 5×5 lattice, i.e. the path
// `M14 68 L32 50 L50 68 L68 32 L86 50` in the 100-unit viewBox, with two
// nodes that carry meaning by shape: the entry RING (amber) where the trace
// begins and the terminal DOT (the ground's action colour) where it ends. No
// middle nodes, no lattice, no shape-swapping.
//
//   mode="static"  — the trace sits drawn.
//   mode="draw"    — boot / resume, once: the ring is present from the first
//                    frame, the path draws on over 420ms at ONE rate, then the
//                    dot fades and scales .4 → 1 in 240ms. No overshoot.
//   mode="loader"  — indeterminate: the whole trace ghosted at 24%, a 60-unit
//                    lit segment travelling it every 1.8s.
//
// `ground` picks the palette: on paper the dot is verdigris and the ring
// amber from the tokens; on the cover (the spine, the auth stage at night)
// the dot is lime and the ring a lifted amber. `mono` drops colour — the ring
// and the dot still carry the meaning. Below 20px the ring is dropped.
//
// Honours prefers-reduced-motion: draw becomes static, the loader a still
// ghost trace.

const PATH = "M14 68 L32 50 L50 68 L68 32 L86 50";
/** The trace's length in viewBox units — four 18√2 diagonals. */
const PATH_LENGTH = 4 * Math.hypot(18, 18);
const ENTRY = { cx: 14, cy: 68, r: 6.5, stroke: 5 };
const TERMINAL = { cx: 86, cy: 50, r: 7.5 };
/** The draw-on. The trace is four equal diagonals, so it draws LINEARLY: under
 * the house ease-out three strokes flicked by in the first quarter of the run
 * and the last one crawled through the remaining three — the final stroke read
 * as slower than the rest. One rate, then the dot's landing is the soft end. */
const DRAW_MS = 420;
const LAND_MS = 240;

export type MarkGround = "paper" | "cover";
export type MarkMode = "static" | "draw" | "loader";

export interface EngramMarkProps {
  size?: number;
  mode?: MarkMode;
  ground?: MarkGround;
  /** One colour only: the shapes carry the meaning (print, monochrome). */
  mono?: boolean;
  title?: string;
  className?: string;
}

/** Stroke width in viewBox units by rendered size: 9 at ≥64px, 10 at 32px,
 * 12 at 16px — heavier as the mark shrinks so the trace stays a line. */
export function strokeFor(size: number): number {
  if (size >= 64) return 9;
  if (size >= 32) return 10;
  return 12;
}

function prefersReducedMotion(): boolean {
  return (
    typeof window !== "undefined" &&
    !!window.matchMedia &&
    window.matchMedia("(prefers-reduced-motion: reduce)").matches
  );
}

/** Whether a draw-on can actually run here: the Web Animations API exists
 * and the person has not asked for reduced motion. When it cannot, the mark
 * renders already drawn — the hidden pre-animation state must never be the
 * resting state. */
function canAnimate(): boolean {
  return (
    typeof Element !== "undefined" &&
    typeof Element.prototype.animate === "function" &&
    !prefersReducedMotion()
  );
}

export function EngramMark({
  size = 30,
  mode = "static",
  ground = "paper",
  mono = false,
  title,
  className,
}: EngramMarkProps) {
  const id = useId();
  const pathRef = useRef<SVGPathElement | null>(null);
  const dotRef = useRef<SVGCircleElement | null>(null);
  const stroke = strokeFor(size);
  const showRing = size >= 20;

  const entryStroke = mono ? "currentColor" : ground === "cover" ? "#e0913d" : "var(--mark-entry)";
  const terminalFill = mono
    ? "currentColor"
    : ground === "cover"
      ? "var(--sidebar-primary)"
      : "var(--mark-terminal)";
  const groundFill = ground === "cover" ? "var(--sidebar)" : "var(--background)";

  // Draw-on happens only where it can; otherwise the mark is simply drawn.
  const willDraw = mode === "draw" && canAnimate();

  // Draw-on: the Web Animations API on the declarative elements, so the
  // static frame is what stays when the animation ends (fill: forwards) and
  // nothing re-runs on re-render.
  useEffect(() => {
    if (!willDraw) return;
    const path = pathRef.current;
    const dot = dotRef.current;
    if (!path || !dot) return;
    const drawing = path.animate([{ strokeDashoffset: PATH_LENGTH }, { strokeDashoffset: 0 }], {
      duration: DRAW_MS,
      easing: "linear",
      fill: "forwards",
    });
    const landing = dot.animate(
      [
        { opacity: 0, transform: "scale(0.4)" },
        { opacity: 1, transform: "scale(1)" },
      ],
      {
        duration: LAND_MS,
        delay: DRAW_MS,
        easing: "cubic-bezier(0.2, 0.7, 0.2, 1)",
        fill: "both",
      },
    );
    return () => {
      drawing.cancel();
      landing.cancel();
    };
  }, [willDraw]);

  const loader = mode === "loader";
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 100 100"
      role="img"
      aria-label={title || "engrams"}
      data-mode={mode}
      className={className}
    >
      {loader && (
        <style>{`@keyframes engram-travel-${id.replace(/:/g, "")} { from { stroke-dashoffset: 120; } to { stroke-dashoffset: -120; } }`}</style>
      )}
      {loader && (
        // The ghost: the whole trace, faint, so the lit segment has a road.
        <path
          d={PATH}
          fill="none"
          stroke="currentColor"
          strokeOpacity={0.24}
          strokeWidth={stroke}
          strokeLinecap="round"
          strokeLinejoin="round"
        />
      )}
      <path
        ref={pathRef}
        d={PATH}
        fill="none"
        stroke="currentColor"
        strokeWidth={stroke}
        strokeLinecap="round"
        strokeLinejoin="round"
        data-slot="trace"
        style={
          loader
            ? {
                strokeDasharray: "60 60",
                strokeDashoffset: 120,
                animation: prefersReducedMotion()
                  ? undefined
                  : `engram-travel-${id.replace(/:/g, "")} 1.8s linear infinite`,
                opacity: prefersReducedMotion() ? 0 : 1,
              }
            : willDraw
              ? { strokeDasharray: PATH_LENGTH, strokeDashoffset: PATH_LENGTH }
              : undefined
        }
      />
      {showRing && !loader && (
        <circle
          cx={ENTRY.cx}
          cy={ENTRY.cy}
          r={ENTRY.r}
          fill={groundFill}
          stroke={entryStroke}
          strokeWidth={ENTRY.stroke}
          data-slot="entry"
        />
      )}
      {!loader && (
        <circle
          ref={dotRef}
          cx={TERMINAL.cx}
          cy={TERMINAL.cy}
          r={TERMINAL.r}
          fill={terminalFill}
          data-slot="terminal"
          style={
            willDraw
              ? { opacity: 0, transformBox: "fill-box", transformOrigin: "center" }
              : undefined
          }
        />
      )}
    </svg>
  );
}
