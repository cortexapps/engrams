import { motion } from 'framer-motion';

// A pull request opened from inside the session (ADR 0023's forge seam)
// — the one durable, reviewable artifact a session hands back. It earns
// a framed notice rather than a chat bubble or a bracketed aside. The
// accent is verdigris, the palette's "lasting / archived" hue (shared
// with snapshots); amber stays reserved for live recency, so it never
// appears here. Square corners + a hairline rule, like everything else.

export interface PullRequestCardProps {
  url: string;
  repo: string;
  title: string;
  /** Provider PR number / MR iid. */
  number: number;
  headBranch: string;
  baseBranch: string;
  at: string;
}

export function PullRequestCard({
  url,
  repo,
  title,
  number,
  headBranch,
  baseBranch,
  at,
}: PullRequestCardProps) {
  return (
    <motion.div
      layout
      initial={{ opacity: 0, y: 4 }}
      animate={{ opacity: 1, y: 0 }}
      transition={{ duration: 0.4, ease: 'easeOut' }}
      className="my-5 px-4 py-3"
      style={{
        backgroundColor: 'var(--color-paper-warm)',
        border: '1px solid var(--color-rule)',
        borderLeft: '2px solid var(--color-verdigris)',
      }}
    >
      {/* Kind label + repo·#number hang on the left; the time on the right. */}
      <div className="flex items-baseline justify-between gap-3">
        <div className="font-mono text-[0.72rem]" style={{ color: 'var(--color-ink-quiet)' }}>
          <span className="glyph" style={{ color: 'var(--color-verdigris)' }}>
            ↳
          </span>{' '}
          <span className="smallcaps" style={{ color: 'var(--color-verdigris)' }}>
            pull request
          </span>
          {' · '}
          {repo} #{number}
        </div>
        <span
          className="font-mono text-[0.72rem]"
          data-tabular
          style={{ color: 'var(--color-ink-quiet)' }}
        >
          {hms(at)}
        </span>
      </div>

      {/* The title links straight to the PR; the ↗ marks it external. */}
      <a
        href={url}
        target="_blank"
        rel="noreferrer"
        className="mt-1.5 block font-display hover:underline"
        style={{ fontSize: '1.05rem', lineHeight: 1.4, color: 'var(--color-ink)' }}
      >
        {title}
        <span style={{ color: 'var(--color-ink-quiet)' }}> ↗</span>
      </a>

      {/* Branch flow, in the same faded mono as tool-call metadata. */}
      <div className="mt-1 font-mono text-[0.78rem]" style={{ color: 'var(--color-ink-quiet)' }}>
        {headBranch}
        <span style={{ color: 'var(--color-ink-faded)' }}> → </span>
        {baseBranch}
      </div>
    </motion.div>
  );
}

// Local mirror of Transcript's time formatter — this card is a
// self-contained block renderer (like ToolCall), so it keeps its own.
function hms(iso: string): string {
  return new Date(iso).toLocaleTimeString('en-GB', {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
  });
}
