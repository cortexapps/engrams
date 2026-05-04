import { Link } from 'react-router-dom';

// "Running header" — fixed top-left of every page, mirroring the
// UserChip in the opposite corner. Click → /. The visual model is a
// running header in book typography (the publication's name printed
// in the same place on every inner page) — quiet, italic, present
// without competing with the page's own H1.
//
// Visible on every route including `/`. Now that the Overview's H1
// is "sessions" (not "engrams"), there's no redundancy: the corner
// carries the brand, the H1 carries the page name. They serve
// different roles and reading both at once is how users navigate.

export function Wordmark() {
  return (
    <Link
      to="/"
      aria-label="back to overview"
      className="fixed top-5 left-5 z-50 select-none transition-colors duration-200"
      style={{
        fontFamily: 'var(--font-display)',
        fontStyle: 'italic',
        fontSize: '1rem',
        lineHeight: 1,
        color: 'var(--color-ink-faded)',
        textUnderlineOffset: '0.2em',
        textDecorationThickness: '1px',
      }}
      onMouseEnter={(e) => {
        const t = e.currentTarget as HTMLElement;
        t.style.color = 'var(--color-ink)';
        t.style.textDecoration = 'underline';
      }}
      onMouseLeave={(e) => {
        const t = e.currentTarget as HTMLElement;
        t.style.color = 'var(--color-ink-faded)';
        t.style.textDecoration = 'none';
      }}
    >
      engrams
    </Link>
  );
}
