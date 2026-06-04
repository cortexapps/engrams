import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';

// The logbook masthead. Every page opens on the same note: a Saira title
// (the display voice, slightly extended for the racing read), an optional
// sans description, and a hairline rule closing the header band — the ruled
// line at the top of a notebook page. A short lime bar sits on that rule like
// an index tab — the accent's one appearance on the header.
// Actions sit opposite the title on the same baseline.
export function PageHeading({
  title,
  description,
  actions,
  className,
}: {
  title: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  className?: string;
}) {
  return (
    <div className={cn('relative flex flex-wrap items-end justify-between gap-x-6 gap-y-3 border-b pb-4', className)}>
      <span aria-hidden className="absolute -bottom-px left-0 h-0.5 w-10 bg-primary" />
      <div className="space-y-1">
        <h1 className="font-display text-3xl font-semibold leading-tight tracking-tight text-balance [font-stretch:108%]">{title}</h1>
        {description && (
          <p className="max-w-prose text-sm text-muted-foreground text-pretty">{description}</p>
        )}
      </div>
      {actions && <div className="flex shrink-0 items-center gap-2">{actions}</div>}
    </div>
  );
}
