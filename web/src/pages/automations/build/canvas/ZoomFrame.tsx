/** The Build tab's viewport: one scale for the whole diagram (every way in,
 * the agent bus, the agent), a scroll area when that scale does not fit, and
 * the zoom controls.
 *
 * The content lays out at its natural size and the frame scales it as one
 * picture, so every node in every column is the same size and nothing is
 * clipped. "Fit" picks the largest scale in [MIN_SCALE, MAX_SCALE] that fits
 * the width; below MIN_SCALE the text is not legible, so the row scrolls.
 * A drag on the empty ground pans the scroll area. */

import { Minus, Plus } from "lucide-react";
import { useEffect, useRef, useState, type PointerEvent, type ReactNode } from "react";

import { Button } from "@/components/ui/button";

import { MAX_SCALE, MIN_SCALE } from "./layout";

const ZOOM_STEP = 0.1;
/** What a pointer-down on the ground must not start a pan from. */
const INTERACTIVE = 'button, a, input, textarea, select, [role="button"], [role="menuitem"]';
/** The scroll area's padding around the diagram: `px-6` and `pt-14 pb-6`. */
const PAD_X = 24;

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

export function ZoomFrame({ toolbar, children }: { toolbar?: ReactNode; children: ReactNode }) {
  const [zoom, setZoom] = useState<number | "fit">("fit");
  const viewportRef = useRef<HTMLDivElement | null>(null);
  const contentRef = useRef<HTMLDivElement | null>(null);
  const [viewportWidth, setViewportWidth] = useState(0);
  const [content, setContent] = useState({ width: 0, height: 0 });

  useEffect(() => {
    const viewport = viewportRef.current;
    const inner = contentRef.current;
    if (!viewport || !inner) return;
    // `offset*` is the layout box, which a transform does not change.
    const update = () => {
      setViewportWidth(viewport.clientWidth);
      setContent({ width: inner.offsetWidth, height: inner.offsetHeight });
    };
    update();
    const observer = new ResizeObserver(update);
    observer.observe(viewport);
    observer.observe(inner);
    return () => observer.disconnect();
  }, []);

  // jsdom has no layout: both widths are 0, and the scale stays 1.
  const available = viewportWidth - 2 * PAD_X;
  const fit =
    available > 0 && content.width > 0 ? clamp(available / content.width, MIN_SCALE, MAX_SCALE) : 1;
  const scale = zoom === "fit" ? fit : zoom;
  const stepZoom = (delta: number) =>
    setZoom(clamp(Math.round((scale + delta) / ZOOM_STEP) * ZOOM_STEP, MIN_SCALE, MAX_SCALE));

  const pan = useRef<{ x: number; y: number; left: number; top: number } | null>(null);
  const onPointerDown = (e: PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0 || e.pointerType === "touch") return;
    if ((e.target as Element).closest(INTERACTIVE)) return;
    // No text selection while the pointer drags across node labels.
    e.preventDefault();
    const viewport = e.currentTarget;
    pan.current = {
      x: e.clientX,
      y: e.clientY,
      left: viewport.scrollLeft,
      top: viewport.scrollTop,
    };
    viewport.setPointerCapture(e.pointerId);
  };
  const onPointerMove = (e: PointerEvent<HTMLDivElement>) => {
    const start = pan.current;
    if (!start) return;
    e.currentTarget.scrollLeft = start.left - (e.clientX - start.x);
    e.currentTarget.scrollTop = start.top - (e.clientY - start.y);
  };
  const endPan = () => {
    pan.current = null;
  };

  return (
    <div className="relative flex min-h-0 flex-1 flex-col">
      <div className="pointer-events-none absolute inset-x-3 top-3 z-30 flex items-start gap-2">
        <div className="pointer-events-auto min-w-0">{toolbar}</div>
        <div
          className="pointer-events-auto ml-auto flex shrink-0 items-center gap-1"
          aria-label="Zoom"
        >
          <Button
            variant="outline"
            size="sm"
            className="h-7 w-7 px-0"
            aria-label="Zoom out"
            disabled={scale <= MIN_SCALE}
            onClick={() => stepZoom(-ZOOM_STEP)}
          >
            <Minus aria-hidden />
          </Button>
          <span
            className="w-10 text-center font-mono text-2xs text-muted-foreground tabular-nums"
            data-testid="zoom-level"
          >
            {Math.round(scale * 100)}%
          </span>
          <Button
            variant="outline"
            size="sm"
            className="h-7 w-7 px-0"
            aria-label="Zoom in"
            disabled={scale >= MAX_SCALE}
            onClick={() => stepZoom(ZOOM_STEP)}
          >
            <Plus aria-hidden />
          </Button>
          <Button
            variant="outline"
            size="sm"
            className="h-7"
            aria-pressed={zoom === "fit"}
            onClick={() => setZoom("fit")}
          >
            Fit
          </Button>
        </div>
      </div>
      <div
        ref={viewportRef}
        className="min-h-0 flex-1 overflow-auto px-6 pt-14 pb-6"
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={endPan}
        onPointerCancel={endPan}
      >
        {/* The sizer takes the scaled size, so the scroll range matches what
            is drawn; `mx-auto` centres a diagram narrower than the view. */}
        <div
          className="mx-auto"
          style={
            content.width > 0
              ? { width: content.width * scale, height: content.height * scale }
              : undefined
          }
        >
          <div
            ref={contentRef}
            className="w-max"
            style={{ transform: `scale(${scale})`, transformOrigin: "top left" }}
          >
            {children}
          </div>
        </div>
      </div>
    </div>
  );
}
