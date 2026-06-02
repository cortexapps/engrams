// The human's turn — a contained, square, flat `paper-warm` block,
// right-aligned, marked with the `§ you` sigil (ADR 0030 §2a). It's the
// familiar "this is you" turn-taking signal from chat UIs, but
// deliberately NOT a rounded bubble: square corners + hairline border +
// flat surface keep it in the lab-notebook system. The assistant's
// reply stays open prose, full-width left; the left/right asymmetry
// reads the dialogue at a glance.

function hms(iso: string): string {
  return new Date(iso).toLocaleTimeString('en-GB', {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
  });
}

export function UserTurn({ prompt, at }: { prompt: string; at?: string }) {
  return (
    <section className="user-turn">
      <div className="user-bubble">
        <div className="user-bubble-head">
          <span className="user-turn-label section-label">
            <span className="user-turn-sigil">§</span> you
          </span>
          {at && (
            <span className="user-bubble-time font-mono" data-tabular>
              {hms(at)}
            </span>
          )}
        </div>
        <p className="user-turn-text">{prompt}</p>
      </div>
    </section>
  );
}
