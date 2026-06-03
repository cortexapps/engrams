// engram-mark.jsx — the logo as a React component, with a living-status mode.
// Renders the "engram trace" mark; can sit STATIC, PULSE once on demand
// (a single strike that settles), or LOOP continuously (booting / resuming).
// Same tuned lightning-strike motion as assets/engram-loader.html.
const { useRef: useMarkRef, useEffect: useMarkEffect } = React;

const MARK_NS = 'http://www.w3.org/2000/svg';
const MARK_COL = { ink: '#1b1612', amber: '#b85c0a', verd: '#3a6b5c', rule: '#d9cfb8', paper: '#f4eedf' };
const MARK_P = (c, r) => [14 + c * 18, 14 + r * 18];
const MARK_TRACES = [
  [[0, 3], [1, 2], [2, 3], [3, 1], [4, 2]],
  [[0, 1], [1, 2], [2, 0], [3, 2], [4, 1]],
  [[0, 2], [1, 4], [2, 2], [3, 3], [4, 1]],
  [[0, 0], [1, 2], [2, 1], [3, 3], [4, 4]],
];
function markEl(tag, attrs) {
  const n = document.createElementNS(MARK_NS, tag);
  for (const k in attrs) n.setAttribute(k, attrs[k]);
  return n;
}

// Imperative driver mounted on a <g> layer. Returns { stop }.
function driveMark(svg, layer, opts) {
  const o = Object.assign({ mode: 'static', period: 2600 }, opts);
  const reduce = window.matchMedia && window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  const grow = o.period * 0.62, hold = o.period * 0.16, fade = o.period * 0.10, gap = o.period * 0.12;
  let cancelled = false, i = 0;

  function paint(trace) {
    layer.innerHTML = '';
    const pts = trace.map(([c, r]) => MARK_P(c, r));
    const d = pts.map((p, k) => (k ? 'L' : 'M') + p[0] + ' ' + p[1]).join(' ');
    const path = markEl('path', { d, fill: 'none', stroke: MARK_COL.ink, 'stroke-width': 1.6, 'stroke-linecap': 'round', 'stroke-linejoin': 'round' });
    layer.appendChild(path);
    const nodes = pts.map((p, k) => {
      const tone = k === 0 ? MARK_COL.amber : k === pts.length - 1 ? MARK_COL.verd : MARK_COL.ink;
      const rad = k === 0 ? 4 : k === pts.length - 1 ? 3.6 : 3;
      const n = markEl('circle', { cx: p[0], cy: p[1], r: rad, fill: tone });
      n.style.transformBox = 'fill-box'; n.style.transformOrigin = 'center';
      layer.appendChild(n);
      return n;
    });
    return { path, nodes };
  }

  // draw the trace fully, statically (no animation)
  function paintStatic(trace) {
    const { path } = paint(trace);
    path.style.strokeDasharray = 'none';
    path.style.strokeDashoffset = '0';
  }

  function strike(trace, onDone, { settle = false } = {}) {
    const { path, nodes } = paint(trace);
    if (reduce) {
      // no motion — fade the whole trace in, then (unless settling) out
      const frames = settle ? [{ opacity: 0 }, { opacity: 1 }] : [{ opacity: 0 }, { opacity: 1 }, { opacity: 1 }, { opacity: 0 }];
      const a = layer.animate(frames, { duration: o.period * 0.9, easing: 'ease-in-out', fill: 'forwards' });
      a.onfinish = () => { if (!cancelled && onDone) onDone(); };
      return;
    }
    const len = path.getTotalLength();
    path.style.strokeDasharray = len;
    path.style.strokeDashoffset = len;
    const da = path.animate([{ strokeDashoffset: len }, { strokeDashoffset: 0 }], { duration: grow, easing: 'linear', fill: 'forwards' });
    const cx = nodes.map((n) => +n.getAttribute('cx')), cy = nodes.map((n) => +n.getAttribute('cy'));
    const cum = [0];
    for (let k = 1; k < nodes.length; k++) cum[k] = cum[k - 1] + Math.hypot(cx[k] - cx[k - 1], cy[k] - cy[k - 1]);
    const tot = cum[cum.length - 1] || 1;
    nodes.forEach((n, k) => {
      const at = (cum[k] / tot) * grow;
      n.animate([{ transform: 'scale(0)', opacity: 0 }, { transform: 'scale(1.3)', opacity: 1, offset: 0.6 }, { transform: 'scale(1)', opacity: 1 }],
        { duration: 130, delay: at, easing: 'cubic-bezier(0.3,0,0,1)', fill: 'backwards' });
    });
    da.onfinish = () => {
      if (cancelled) return;
      if (settle) { if (onDone) onDone(); return; }   // leave the bolt drawn
      const fl = layer.animate([{ opacity: 1, offset: 0 }, { opacity: 1, offset: hold / (hold + fade) }, { opacity: 0, offset: 1 }],
        { duration: hold + fade, easing: 'cubic-bezier(0.7,0,0.84,0)', fill: 'forwards' });
      fl.onfinish = () => { if (!cancelled && onDone) onDone(); };
    };
  }

  if (o.mode === 'static') {
    paintStatic(MARK_TRACES[0]);
  } else if (o.mode === 'loop') {
    const cycle = () => { if (cancelled) return; const t = MARK_TRACES[i]; i = (i + 1) % MARK_TRACES.length; strike(t, () => { if (!cancelled) setTimeout(cycle, gap); }); };
    cycle();
  } else if (o.mode === 'pulse') {
    // a single strike that settles to a drawn (static) trace
    strike(MARK_TRACES[i % MARK_TRACES.length], null, { settle: true });
  }
  return { stop() { cancelled = true; } };
}

// React wrapper. `mode`: 'static' | 'loop'. `pulseKey`: when this value
// changes (and mode !== 'loop'), fire one strike on the base trace.
// The STATIC baseline (path + nodes) is rendered DECLARATIVELY as JSX so it
// always paints (and survives DOM-clone screenshotting); only the loop and
// the pulse strikes are driven imperatively over those same elements.
function EngramMark({ size = 30, frame = false, substrate = false, mode = 'static', pulseKey = 0, period = 2600, title }) {
  const svgRef = useMarkRef(null);
  const baseRef = useMarkRef(null);   // <g> holding the declarative base trace
  const pathRef = useMarkRef(null);
  const nodeRefs = useMarkRef([]);
  const loopRef = useMarkRef(null);   // <g> the imperative loop paints into

  const base = MARK_TRACES[0];
  const basePts = base.map(([c, r]) => MARK_P(c, r));
  const baseD = basePts.map((p, k) => (k ? 'L' : 'M') + p[0] + ' ' + p[1]).join(' ');
  const toneOf = (k) => (k === 0 ? MARK_COL.amber : k === basePts.length - 1 ? MARK_COL.verd : MARK_COL.ink);
  const radOf = (k) => (k === 0 ? 4 : k === basePts.length - 1 ? 3.6 : 3);

  const subs = [];
  if (substrate) {
    const on = new Set(base.map(([c, r]) => `${c},${r}`));
    for (let c = 0; c < 5; c++) for (let r = 0; r < 5; r++) if (!on.has(`${c},${r}`)) subs.push(MARK_P(c, r));
  }

  // loop: hide the declarative base, cycle the four traces imperatively
  useMarkEffect(() => {
    if (mode !== 'loop' || !svgRef.current || !loopRef.current) return;
    if (baseRef.current) baseRef.current.style.display = 'none';
    const d = driveMark(svgRef.current, loopRef.current, { mode: 'loop', period });
    return () => { d.stop(); if (loopRef.current) loopRef.current.innerHTML = ''; if (baseRef.current) baseRef.current.style.display = ''; };
  }, [mode, period]);

  // pulse: re-strike the (declarative) base trace whenever pulseKey changes
  useMarkEffect(() => {
    if (mode === 'loop' || !pulseKey || !pathRef.current) return;
    const reduce = window.matchMedia && window.matchMedia('(prefers-reduced-motion: reduce)').matches;
    const path = pathRef.current, nodes = nodeRefs.current.filter(Boolean);
    if (reduce) return;   // leave the static trace as-is, no motion
    const grow = period * 0.62;
    const len = path.getTotalLength();
    path.animate([{ strokeDashoffset: len }, { strokeDashoffset: 0 }], { duration: grow, easing: 'linear' });
    const cx = nodes.map((n) => +n.getAttribute('cx')), cy = nodes.map((n) => +n.getAttribute('cy'));
    const cum = [0];
    for (let k = 1; k < nodes.length; k++) cum[k] = cum[k - 1] + Math.hypot(cx[k] - cx[k - 1], cy[k] - cy[k - 1]);
    const tot = cum[cum.length - 1] || 1;
    nodes.forEach((n, k) => {
      const at = (cum[k] / tot) * grow;
      n.animate([{ transform: 'scale(0.2)', opacity: 0.2 }, { transform: 'scale(1.3)', opacity: 1, offset: 0.6 }, { transform: 'scale(1)', opacity: 1 }],
        { duration: 200, delay: at, easing: 'cubic-bezier(0.3,0,0,1)' });
    });
  }, [pulseKey]);

  return (
    <svg ref={svgRef} width={size} height={size} viewBox="0 0 100 100" role="img" aria-label={title || 'engrams'}>
      {frame && <rect x="3" y="3" width="94" height="94" fill="none" stroke={MARK_COL.ink} strokeWidth="1.4" />}
      {subs.map(([x, y], k) => <circle key={k} cx={x} cy={y} r="1.6" fill={MARK_COL.paper} stroke={MARK_COL.rule} strokeWidth="1" />)}
      <g ref={baseRef}>
        <path ref={pathRef} d={baseD} fill="none" stroke={MARK_COL.ink} strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" />
        {basePts.map((p, k) => (
          <circle key={k} ref={(el) => { nodeRefs.current[k] = el; }} cx={p[0]} cy={p[1]} r={radOf(k)} fill={toneOf(k)}
            style={{ transformBox: 'fill-box', transformOrigin: 'center' }} />
        ))}
      </g>
      <g ref={loopRef} />
    </svg>
  );
}

Object.assign(window, { EngramMark, MARK_TRACES });
