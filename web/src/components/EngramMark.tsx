import { useEffect, useRef } from 'react';

// The "engram trace" logomark as a React component, with a living
// status mode. Renders a sparse memory trace on a 5×5 graph-paper
// lattice: an amber entry node ("now"), an ink path, a verdigris
// consolidated terminal. The STATIC baseline (path + nodes) is drawn
// declaratively as JSX so it always paints; only the loop and the
// per-tick pulse strikes are driven imperatively over those same
// elements via the Web Animations API.
//
//   mode="static" — the trace sits drawn.
//   mode="loop"   — booting/resuming: cycle the four traces, each
//                   growing out edge-by-edge, holding, snapping off.
//   pulseKey      — when this value changes (and mode !== 'loop'),
//                   fire ONE strike on the base trace. Drive it off
//                   the poll tick for a "heartbeat per refetch".
//
// Honors `prefers-reduced-motion`: the loop cross-fades whole traces
// and the pulse is a no-op (the static trace stays put).

const COL = {
  ink: '#1b1612',
  amber: '#b85c0a',
  verd: '#3a6b5c',
  rule: '#d9cfb8',
  paper: '#f4eedf',
} as const;

type Cell = [number, number];

// graph-paper coordinate for a (col,row) lattice cell, in the 0..100 viewBox
const P = (c: number, r: number): [number, number] => [14 + c * 18, 14 + r * 18];

const TRACES: Cell[][] = [
  [
    [0, 3],
    [1, 2],
    [2, 3],
    [3, 1],
    [4, 2],
  ],
  [
    [0, 1],
    [1, 2],
    [2, 0],
    [3, 2],
    [4, 1],
  ],
  [
    [0, 2],
    [1, 4],
    [2, 2],
    [3, 3],
    [4, 1],
  ],
  [
    [0, 0],
    [1, 2],
    [2, 1],
    [3, 3],
    [4, 4],
  ],
];

const NS = 'http://www.w3.org/2000/svg';

function el(tag: string, attrs: Record<string, string | number>): SVGElement {
  const n = document.createElementNS(NS, tag);
  for (const k in attrs) n.setAttribute(k, String(attrs[k]));
  return n;
}

function prefersReducedMotion(): boolean {
  return (
    typeof window !== 'undefined' &&
    !!window.matchMedia &&
    window.matchMedia('(prefers-reduced-motion: reduce)').matches
  );
}

// Imperative driver mounted on a <g> layer. Returns a stop handle.
function driveLoop(layer: SVGGElement, period = 2600): { stop: () => void } {
  const reduce = prefersReducedMotion();
  const grow = period * 0.62;
  const hold = period * 0.16;
  const fade = period * 0.1;
  const gap = period * 0.12;
  let cancelled = false;
  let i = 0;

  function paint(trace: Cell[]) {
    layer.innerHTML = '';
    const pts = trace.map(([c, r]) => P(c, r));
    const d = pts.map((p, k) => (k ? 'L' : 'M') + p[0] + ' ' + p[1]).join(' ');
    const path = el('path', {
      d,
      fill: 'none',
      stroke: COL.ink,
      'stroke-width': 1.6,
      'stroke-linecap': 'round',
      'stroke-linejoin': 'round',
    }) as SVGPathElement;
    layer.appendChild(path);
    const nodes = pts.map((p, k) => {
      const tone =
        k === 0 ? COL.amber : k === pts.length - 1 ? COL.verd : COL.ink;
      const rad = k === 0 ? 4 : k === pts.length - 1 ? 3.6 : 3;
      const n = el('circle', { cx: p[0], cy: p[1], r: rad, fill: tone });
      (n as SVGElement).style.transformBox = 'fill-box';
      (n as SVGElement).style.transformOrigin = 'center';
      layer.appendChild(n);
      return n as SVGCircleElement;
    });
    return { path, nodes };
  }

  function strike(trace: Cell[], onDone: () => void) {
    const { path, nodes } = paint(trace);
    if (reduce) {
      const a = layer.animate(
        [{ opacity: 0 }, { opacity: 1 }, { opacity: 1 }, { opacity: 0 }],
        { duration: period * 0.9, easing: 'ease-in-out', fill: 'forwards' },
      );
      a.onfinish = () => {
        if (!cancelled) onDone();
      };
      return;
    }
    const len = path.getTotalLength();
    path.style.strokeDasharray = String(len);
    path.style.strokeDashoffset = String(len);
    const da = path.animate(
      [{ strokeDashoffset: len }, { strokeDashoffset: 0 }],
      { duration: grow, easing: 'linear', fill: 'forwards' },
    );
    const cx = nodes.map((n) => +n.getAttribute('cx')!);
    const cy = nodes.map((n) => +n.getAttribute('cy')!);
    const cum = [0];
    for (let k = 1; k < nodes.length; k++)
      cum[k] = cum[k - 1] + Math.hypot(cx[k] - cx[k - 1], cy[k] - cy[k - 1]);
    const tot = cum[cum.length - 1] || 1;
    nodes.forEach((n, k) => {
      const at = (cum[k] / tot) * grow;
      n.animate(
        [
          { transform: 'scale(0)', opacity: 0 },
          { transform: 'scale(1.3)', opacity: 1, offset: 0.6 },
          { transform: 'scale(1)', opacity: 1 },
        ],
        {
          duration: 130,
          delay: at,
          easing: 'cubic-bezier(0.3,0,0,1)',
          fill: 'backwards',
        },
      );
    });
    da.onfinish = () => {
      if (cancelled) return;
      const fl = layer.animate(
        [
          { opacity: 1, offset: 0 },
          { opacity: 1, offset: hold / (hold + fade) },
          { opacity: 0, offset: 1 },
        ],
        { duration: hold + fade, easing: 'cubic-bezier(0.7,0,0.84,0)', fill: 'forwards' },
      );
      fl.onfinish = () => {
        if (!cancelled) onDone();
      };
    };
  }

  const cycle = () => {
    if (cancelled) return;
    const t = TRACES[i];
    i = (i + 1) % TRACES.length;
    strike(t, () => {
      if (!cancelled) window.setTimeout(cycle, gap);
    });
  };
  cycle();

  return {
    stop() {
      cancelled = true;
    },
  };
}

export interface EngramMarkProps {
  size?: number;
  /** Draw a 1px square frame around the lattice (badge/favicon look). */
  frame?: boolean;
  mode?: 'static' | 'loop';
  /** Change this value to fire one pulse strike (ignored in loop mode). */
  pulseKey?: number;
  period?: number;
  title?: string;
}

export function EngramMark({
  size = 30,
  frame = false,
  mode = 'static',
  pulseKey = 0,
  period = 2600,
  title,
}: EngramMarkProps) {
  const baseRef = useRef<SVGGElement | null>(null);
  const pathRef = useRef<SVGPathElement | null>(null);
  const nodeRefs = useRef<(SVGCircleElement | null)[]>([]);
  const loopRef = useRef<SVGGElement | null>(null);

  const base = TRACES[0];
  const basePts = base.map(([c, r]) => P(c, r));
  const baseD = basePts
    .map((p, k) => (k ? 'L' : 'M') + p[0] + ' ' + p[1])
    .join(' ');
  const toneOf = (k: number) =>
    k === 0 ? COL.amber : k === basePts.length - 1 ? COL.verd : COL.ink;
  const radOf = (k: number) => (k === 0 ? 4 : k === basePts.length - 1 ? 3.6 : 3);

  // loop: hide the declarative base, cycle the four traces imperatively
  useEffect(() => {
    if (mode !== 'loop' || !loopRef.current) return;
    if (baseRef.current) baseRef.current.style.display = 'none';
    const d = driveLoop(loopRef.current, period);
    return () => {
      d.stop();
      if (loopRef.current) loopRef.current.innerHTML = '';
      if (baseRef.current) baseRef.current.style.display = '';
    };
  }, [mode, period]);

  // pulse: re-strike the declarative base trace whenever pulseKey changes
  useEffect(() => {
    if (mode === 'loop' || !pulseKey || !pathRef.current) return;
    if (prefersReducedMotion()) return;
    const path = pathRef.current;
    const nodes = nodeRefs.current.filter(Boolean) as SVGCircleElement[];
    const grow = period * 0.62;
    const len = path.getTotalLength();
    path.animate([{ strokeDashoffset: len }, { strokeDashoffset: 0 }], {
      duration: grow,
      easing: 'linear',
    });
    const cx = nodes.map((n) => +n.getAttribute('cx')!);
    const cy = nodes.map((n) => +n.getAttribute('cy')!);
    const cum = [0];
    for (let k = 1; k < nodes.length; k++)
      cum[k] = cum[k - 1] + Math.hypot(cx[k] - cx[k - 1], cy[k] - cy[k - 1]);
    const tot = cum[cum.length - 1] || 1;
    nodes.forEach((n, k) => {
      const at = (cum[k] / tot) * grow;
      n.animate(
        [
          { transform: 'scale(0.2)', opacity: 0.2 },
          { transform: 'scale(1.3)', opacity: 1, offset: 0.6 },
          { transform: 'scale(1)', opacity: 1 },
        ],
        { duration: 200, delay: at, easing: 'cubic-bezier(0.3,0,0,1)' },
      );
    });
  }, [pulseKey, mode, period]);

  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 100 100"
      role="img"
      aria-label={title || 'engrams'}
    >
      {frame && (
        <rect
          x="3"
          y="3"
          width="94"
          height="94"
          fill="none"
          stroke={COL.ink}
          strokeWidth="1.4"
        />
      )}
      <g ref={baseRef}>
        <path
          ref={pathRef}
          d={baseD}
          fill="none"
          stroke={COL.ink}
          strokeWidth="1.6"
          strokeLinecap="round"
          strokeLinejoin="round"
        />
        {basePts.map((p, k) => (
          <circle
            key={k}
            ref={(node) => {
              nodeRefs.current[k] = node;
            }}
            cx={p[0]}
            cy={p[1]}
            r={radOf(k)}
            fill={toneOf(k)}
            style={{ transformBox: 'fill-box', transformOrigin: 'center' }}
          />
        ))}
      </g>
      <g ref={loopRef} />
    </svg>
  );
}
