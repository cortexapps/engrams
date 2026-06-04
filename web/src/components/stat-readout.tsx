import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';

export interface Stat {
  label: string;
  value: ReactNode;
}

// An instrument readout, not a row of cards. Figures are mono and oversized,
// labels are small mono caps beneath them, and a single hairline grid (drawn
// with a 1px gap over the border color) separates the cells like a gauge
// cluster. Responsive by column count passed in `className`.
export function StatReadout({ items, className }: { items: Stat[]; className?: string }) {
  return (
    <dl
      className={cn(
        'grid grid-cols-2 gap-px overflow-hidden rounded-md border bg-border sm:grid-cols-4',
        className,
      )}
    >
      {items.map(({ label, value }) => (
        <div key={label} className="bg-card px-4 py-3">
          <dd className="font-mono text-2xl leading-none tabular-nums">{value}</dd>
          <dt className="mt-1.5 font-mono text-[0.65rem] uppercase tracking-[0.14em] text-muted-foreground">
            {label}
          </dt>
        </div>
      ))}
    </dl>
  );
}
