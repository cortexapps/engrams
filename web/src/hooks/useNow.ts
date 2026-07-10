import { useEffect, useState } from "react";

/** A lightweight render clock for relative-time labels. */
export function useNow(ms = 30_000): number {
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    const interval = window.setInterval(() => setNow(Date.now()), ms);
    return () => window.clearInterval(interval);
  }, [ms]);

  return now;
}
