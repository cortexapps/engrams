// Typeset tab labels with a hairline underline below the active one.
// Not pill buttons. Not icons. Just labels in mono small-caps with a
// 1px solid amber rule under whichever is selected. Switching is
// instant — tabs are state, not motion.

export interface Tab<T extends string> {
  id: T;
  label: string;
}

export interface TabRowProps<T extends string> {
  tabs: Tab<T>[];
  active: T;
  onChange: (id: T) => void;
  /** Optional element rendered to the right of the tabs (e.g. counts). */
  right?: React.ReactNode;
}

export function TabRow<T extends string>({
  tabs,
  active,
  onChange,
  right,
}: TabRowProps<T>) {
  return (
    <div
      className="flex items-baseline justify-between mb-4 pb-2"
      style={{ borderBottom: '1px solid var(--color-rule)' }}
    >
      <nav className="flex items-baseline gap-5">
        {tabs.map((t) => {
          const isActive = t.id === active;
          return (
            <button
              key={t.id}
              type="button"
              onClick={() => onChange(t.id)}
              className="font-mono smallcaps text-[0.7rem] transition-colors"
              style={{
                color: isActive ? 'var(--color-ink)' : 'var(--color-ink-quiet)',
                letterSpacing: '0.18em',
                paddingBottom: '0.3rem',
                marginBottom: '-0.5rem', // bring the underline flush with the rule
                borderBottom: isActive
                  ? '1px solid var(--color-amber)'
                  : '1px solid transparent',
              }}
            >
              {t.label}
            </button>
          );
        })}
      </nav>
      {right && (
        <div
          className="font-mono text-[0.7rem]"
          style={{ color: 'var(--color-ink-quiet)' }}
        >
          {right}
        </div>
      )}
    </div>
  );
}
