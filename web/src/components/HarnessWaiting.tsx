import { EngramMark } from './EngramMark';

// The harness is working — shown where the assistant's reply will land,
// in the transcript flow, while we wait for it to speak (ADR 0030 §2e).
// A looping engram trace (the mark forming = a thought forming) beside a
// context-aware verb derived from whatever's actually in flight
// ("running cargo check…", "reading snapshot_uffd.rs…"), falling back to
// a generic gerund between turns. Unmounts the instant the real message
// arrives.
//
// `onStop`, when provided, surfaces a `✕ stop` control on this line —
// the operator interrupt (ADR 0030 §3).

export function HarnessWaiting({
  verb = 'thinking',
  onStop,
}: {
  verb?: string;
  onStop?: () => void;
}) {
  return (
    <section className="harness-waiting relative">
      <span className="margin-note margin-left smallcaps">assistant.</span>
      <div className="thinking">
        <span className="thinking-mark">
          <EngramMark size={20} mode="loop" period={2200} />
        </span>
        <span className="thinking-verb section-label">{verb}…</span>
        {onStop && (
          <button
            type="button"
            className="stop-btn section-label"
            onClick={onStop}
          >
            ✕ stop
          </button>
        )}
      </div>
    </section>
  );
}
