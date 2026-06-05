import type { ReactNode } from 'react';
import { cn } from '@/lib/utils';
import { Text } from '@/components/ui/text';

// The logbook masthead. Every page opens on the same note: a Saira title
// (the display voice, slightly extended for the racing read), an optional
// sans description, and a hairline rule closing the header band — the ruled
// line at the top of a notebook page. A short lime bar sits on that rule like
// an index tab — the accent's one appearance on the header.
// Actions sit opposite the title on the same baseline.
//
// `eyebrow` adds a small Saira-caps kicker above the title, for pages that
// name their subject (a session, a host). `titleVariant="mono"` swaps the
// display voice for the lab-readout voice when the title IS machine data (an
// id, a digest) rather than prose — the same masthead frame, honest type.
export function PageHeading({
  title,
  eyebrow,
  description,
  actions,
  titleVariant = 'display',
  className,
}: {
  title: ReactNode;
  eyebrow?: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  titleVariant?: 'display' | 'mono';
  className?: string;
}) {
  return (
    <div className={cn('relative flex flex-wrap items-end justify-between gap-x-6 gap-y-3 border-b pb-4', className)}>
      <span aria-hidden className="absolute -bottom-px left-0 h-0.5 w-10 bg-primary" />
      <div className="space-y-1">
        {eyebrow && <Text variant="label" tone="muted">{eyebrow}</Text>}
        <Text as="h1" variant={titleVariant === 'mono' ? 'displayMono' : 'display'}>
          {title}
        </Text>
        {description && (
          <p className="max-w-prose text-sm text-muted-foreground text-pretty">{description}</p>
        )}
      </div>
      {actions && <div className="flex shrink-0 items-center gap-2">{actions}</div>}
    </div>
  );
}
