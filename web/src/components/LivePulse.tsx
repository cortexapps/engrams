import { StatusDot } from "./status-dot";

/**
 * The "this is happening right now" dot: the running tone breathing at the
 * system's one live cadence (2.4s), never a ping ring. It is decoration only:
 * every caller pairs it with a word.
 */
export function LivePulse({ className }: { className?: string }) {
  return <StatusDot tone="active" size={6} className={className} />;
}
