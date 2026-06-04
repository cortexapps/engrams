import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';

// The logbook masthead. Every page opens on the same note: a serif title
// (the identity voice), an optional sans description, and a hairline rule
// closing the header band — the ruled line at the top of a notebook page.
// Actions (primary buttons) sit opposite the title on the same baseline.
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
    <div className={cn('flex flex-wrap items-end justify-between gap-x-6 gap-y-3 border-b pb-4', className)}>
      <div className="space-y-1">
        <h1 className="font-serif text-3xl font-medium leading-tight text-balance">{title}</h1>
        {description && (
          <p className="max-w-prose text-sm text-muted-foreground text-pretty">{description}</p>
        )}
      </div>
      {actions && <div className="flex shrink-0 items-center gap-2">{actions}</div>}
    </div>
  );
}
