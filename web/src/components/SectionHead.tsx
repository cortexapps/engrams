// A typeset section header: mono small-caps label over a hairline
// rule. Shared by the inline forms and any surface that wants the
// classic ledger-section heading (the grouped manifest uses its own
// `.manifest-group-head` with a trailing count instead).

export function SectionHead({ label }: { label: string }) {
  return (
    <h2
      className="font-mono smallcaps text-[0.7rem] mb-4 pb-2"
      style={{
        color: 'var(--color-ink-quiet)',
        borderBottom: '1px solid var(--color-rule)',
        letterSpacing: '0.18em',
      }}
    >
      {label}
    </h2>
  );
}
