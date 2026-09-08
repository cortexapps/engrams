import { useEffect, useRef, useState, type ReactNode } from "react";

import { cn } from "@/lib/utils";

// A figure that ticks when it changes: the old value slides up and out, the
// new one slides in from below, inside a 1em clip — an odometer, not a flash.
// 500ms; instant under prefers-reduced-motion (the CSS drops the transform).
export function TickNumber({ value, className }: { value: ReactNode; className?: string }) {
  const key = String(value);
  const [shown, setShown] = useState<{ key: string; value: ReactNode; prev: ReactNode | null }>({
    key,
    value,
    prev: null,
  });
  const timer = useRef<number | null>(null);

  useEffect(() => {
    if (key === shown.key) return;
    setShown((s) => ({ key, value, prev: s.value }));
    if (timer.current) window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setShown((s) => ({ ...s, prev: null })), 520);
    return () => {
      if (timer.current) window.clearTimeout(timer.current);
    };
    // `shown.key` is read to decide, not depended on: only a new value ticks.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);

  return (
    <span
      className={cn(
        "relative inline-block h-[1em] overflow-hidden align-baseline leading-none",
        className,
      )}
      data-slot="tick-number"
    >
      <span key={shown.key} className={cn("block", shown.prev !== null && "tick-in")}>
        {shown.value}
      </span>
      {shown.prev !== null && (
        <span aria-hidden className="tick-out absolute inset-x-0 top-0 block">
          {shown.prev}
        </span>
      )}
    </span>
  );
}
