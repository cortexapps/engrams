import { useEffect, useState } from "react";
import { cn } from "@/lib/utils";
import { Text } from "@/components/ui/text";

// A radial instrument gauge — the cockpit's signature element. A 270° dial
// (gap at the bottom, like a tachometer) with a coloured zone face, a needle
// that sweeps to the value on mount, and the figure read out in the center in
// the same mono/Saira instrument voice the rest of the app uses.
//
// Status colour lives in the dial, never the accent: the zone the needle lands
// in tints the needle and the figure, so a red needle reads as "redline" at a
// glance. `value === null` means no data — the needle parks at zero, muted.

export type Tone = "nominal" | "caution" | "critical";
export interface Zone {
  /** Upper bound of this band, as a percentage of `max` (0–100). */
  to: number;
  tone: Tone;
}

const SWEEP = 270; // degrees of arc; the missing 90° is the bottom gap
const START = -135; // needle angle at value 0 (lower-left), clockwise-positive

const STROKE: Record<Tone, string> = {
  nominal: "stroke-instrument-nominal",
  caution: "stroke-instrument-caution",
  critical: "stroke-instrument-critical",
};
const ZONE_STROKE: Record<Tone, string> = {
  nominal: "stroke-instrument-nominal/30",
  caution: "stroke-instrument-caution/30",
  critical: "stroke-instrument-critical/30",
};
const FILL: Record<Tone, string> = {
  nominal: "fill-instrument-nominal",
  caution: "fill-instrument-caution",
  critical: "fill-instrument-critical",
};
const FIGURE: Record<Tone, string> = {
  nominal: "text-instrument-nominal",
  caution: "text-instrument-caution",
  critical: "text-instrument-critical",
};

const CX = 50;
const CY = 50;
const R = 38;

// Angle (degrees, clockwise from straight-up) for a fraction 0–1 of the sweep.
const frac2deg = (f: number) => START + Math.min(1, Math.max(0, f)) * SWEEP;

function polar(angleDeg: number, r: number) {
  const a = (angleDeg * Math.PI) / 180;
  return { x: CX + r * Math.sin(a), y: CY - r * Math.cos(a) };
}

function arcPath(fromDeg: number, toDeg: number, r: number) {
  const s = polar(fromDeg, r);
  const e = polar(toDeg, r);
  const large = Math.abs(toDeg - fromDeg) > 180 ? 1 : 0;
  return `M ${s.x.toFixed(2)} ${s.y.toFixed(2)} A ${r} ${r} 0 ${large} 1 ${e.x.toFixed(2)} ${e.y.toFixed(2)}`;
}

function toneFor(pct: number, zones: Zone[]): Tone {
  for (const z of zones) if (pct <= z.to) return z.tone;
  return zones[zones.length - 1]?.tone ?? "nominal";
}

export function Gauge({
  value,
  max = 100,
  unit = "%",
  label,
  zones,
  className,
}: {
  value: number | null;
  max?: number;
  unit?: string;
  label: string;
  zones: Zone[];
  className?: string;
}) {
  const hasValue = value != null && Number.isFinite(value);
  const pct =
    value != null && Number.isFinite(value) ? Math.min(100, Math.max(0, (value / max) * 100)) : 0;
  const tone = toneFor(pct, zones);
  const target = frac2deg(pct / 100);
  const FULL = START + SWEEP;

  // Ignition self-test: like a racecar cluster powering on, the needle parks
  // at zero, rushes to the redline, holds a beat, then settles to the live
  // reading. The transition lives in the class (not inline) so motion-reduce
  // can cancel it — reduced-motion users jump straight to the value, no sweep.
  const [angle, setAngle] = useState(START);
  const [phase, setPhase] = useState<"sweep" | "settle">("sweep");
  useEffect(() => {
    if (!hasValue) return; // nothing to read — needle stays parked
    const reduce =
      typeof window !== "undefined" && window.matchMedia
        ? window.matchMedia("(prefers-reduced-motion: reduce)").matches
        : false;
    if (reduce) {
      setPhase("settle");
      setAngle(target);
      return;
    }
    setPhase("sweep");
    setAngle(FULL); // rush to the redline
    const t = setTimeout(() => {
      setPhase("settle");
      setAngle(target); // ease back down to the reading
    }, 620);
    return () => clearTimeout(t);
  }, [target, hasValue, FULL]);

  // Zone band: consecutive arcs, each from the previous bound to its own.
  let cursor = 0;

  return (
    <div
      className={cn(
        "relative flex aspect-[10/9] w-full max-w-[220px] items-center justify-center",
        className,
      )}
    >
      <svg
        viewBox="0 0 100 90"
        className="h-full w-full"
        role="img"
        aria-label={`${label}: ${hasValue ? `${Math.round(pct)}${unit}` : "no data"}`}
      >
        {/* dial face: the unlit track */}
        <path
          d={arcPath(START, START + SWEEP, R)}
          fill="none"
          strokeWidth={7}
          strokeLinecap="round"
          className="stroke-border"
        />
        {/* coloured zone bands over the track */}
        {zones.map((z) => {
          const seg = arcPath(frac2deg(cursor / 100), frac2deg(z.to / 100), R);
          cursor = z.to;
          return (
            <path
              key={z.tone + z.to}
              d={seg}
              fill="none"
              strokeWidth={7}
              className={ZONE_STROKE[z.tone]}
            />
          );
        })}
        {/* tick at each zone boundary — the redline marks */}
        {zones.slice(0, -1).map((z) => {
          const a = polar(frac2deg(z.to / 100), R + 4);
          const b = polar(frac2deg(z.to / 100), R - 4);
          return (
            <line
              key={`tick-${z.to}`}
              x1={a.x}
              y1={a.y}
              x2={b.x}
              y2={b.y}
              strokeWidth={1}
              className="stroke-border"
            />
          );
        })}
        {/* needle + hub: tone-coloured when live, muted when no data */}
        <g
          className={cn(
            "origin-center [transform-box:view-box] transition-transform ease-[cubic-bezier(0.16,1,0.3,1)] motion-reduce:transition-none",
            phase === "sweep" ? "duration-[500ms]" : "duration-[850ms]",
          )}
          style={{ transform: `rotate(${angle}deg)`, transformOrigin: "50px 50px" }}
        >
          <line
            x1={CX}
            y1={CY}
            x2={CX}
            y2={CY - (R - 4)}
            strokeWidth={2.5}
            strokeLinecap="round"
            className={hasValue ? STROKE[tone] : "stroke-muted-foreground/50"}
          />
        </g>
        <circle
          cx={CX}
          cy={CY}
          r={3.5}
          className={hasValue ? FILL[tone] : "fill-muted-foreground/50"}
        />
      </svg>

      {/* Readout: the figure owns the dial face (centred on the hub); the
          caption drops into the dial's bottom gap on a filled nameplate, so it
          reads clear of the coloured bands instead of losing contrast against
          them. */}
      <div className="pointer-events-none absolute inset-0">
        <div className="absolute inset-x-0 top-[47%] flex -translate-y-1/2 justify-center">
          <Text
            as="span"
            variant="stat"
            className={cn("text-3xl", hasValue ? FIGURE[tone] : "text-muted-foreground")}
          >
            {hasValue ? Math.round(pct) : "—"}
            {hasValue && <span className="ml-0.5 align-top text-base">{unit}</span>}
          </Text>
        </div>
        <Text
          as="span"
          variant="label"
          className="absolute bottom-[2%] left-1/2 -translate-x-1/2 rounded-md border border-border/60 bg-muted px-2 py-0.5 text-[0.62rem] text-foreground/75"
        >
          {label}
        </Text>
      </div>
    </div>
  );
}
