/**
 * The interpunct between two metadata facts — quieter than the facts it
 * separates, and never announced.
 *
 * It lives here because the reviews surfaces set the same run of facts five
 * different ways (the ledger row, the finding card, the PR readout, the pass
 * switcher), and a separator that drifts in weight between them reads as four
 * different kinds of pause.
 */
export function Sep() {
  return (
    <span aria-hidden className="shrink-0 select-none text-muted-foreground/40">
      ·
    </span>
  );
}

/** A LEADING separator, so a readout can drop segments without stranding one. */
export function Dot({ show, children }: { show: boolean; children: React.ReactNode }) {
  return (
    <span className="flex items-baseline gap-2">
      {show && <Sep />}
      {children}
    </span>
  );
}
