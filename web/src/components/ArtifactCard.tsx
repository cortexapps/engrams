import { motion } from 'framer-motion';

// A file artifact shared from inside the session (ADR 0026) — an agent
// screenshot/recording, or an operator file pull. Like the PR card it's
// a durable thing the session hands back, so it earns a framed notice
// with the same verdigris "lasting / archived" accent. Media renders
// inline (it's the whole point — show, don't describe); anything else
// gets a download chip and is never rendered inline (the bytes are
// attacker-controlled; the coord serves them with nosniff +
// Content-Disposition + a sandbox CSP, and we never navigate to them).

export interface ArtifactCardProps {
  sessionId: string;
  artifactId: string;
  mediaType: string;
  sizeBytes: number;
  caption: string | null;
  at: string;
}

export function ArtifactCard({
  sessionId,
  artifactId,
  mediaType,
  sizeBytes,
  caption,
  at,
}: ArtifactCardProps) {
  // Same-origin; the browser carries the IAP cookie (prod) / Vite proxy
  // (dev). No bearer needed for a passive <img>/<video> GET.
  const src = `/sessions/${sessionId}/artifacts/${artifactId}`;
  const isImage = mediaType.startsWith('image/');
  const isVideo = mediaType.startsWith('video/');

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
      <div className="flex items-baseline justify-between gap-3">
        <div className="font-mono text-[0.72rem]" style={{ color: 'var(--color-ink-quiet)' }}>
          <span className="glyph" style={{ color: 'var(--color-verdigris)' }}>
            ↳
          </span>{' '}
          <span className="smallcaps" style={{ color: 'var(--color-verdigris)' }}>
            shared file
          </span>
          {' · '}
          {mediaType} · {formatBytes(sizeBytes)}
        </div>
        <span
          className="font-mono text-[0.72rem]"
          data-tabular
          style={{ color: 'var(--color-ink-quiet)' }}
        >
          {hms(at)}
        </span>
      </div>

      {/* The artifact itself: media inline, everything else a download chip. */}
      <div className="mt-2">
        {isImage ? (
          <img
            src={src}
            alt={caption ?? 'shared image'}
            className="block max-w-full"
            style={{ maxHeight: '32rem', border: '1px solid var(--color-rule)' }}
          />
        ) : isVideo ? (
          // eslint-disable-next-line jsx-a11y/media-has-caption
          <video
            src={src}
            controls
            className="block max-w-full"
            style={{ maxHeight: '32rem', border: '1px solid var(--color-rule)' }}
          />
        ) : (
          <a
            href={src}
            download
            className="inline-block font-mono text-[0.82rem] hover:underline"
            style={{ color: 'var(--color-verdigris)' }}
          >
            ⤓ download file
          </a>
        )}
      </div>

      {/* Caption is untrusted text — rendered as plain JSX (auto-escaped). */}
      {caption ? (
        <div
          className="mt-1.5 font-display"
          style={{ fontSize: '0.95rem', lineHeight: 1.4, color: 'var(--color-ink)' }}
        >
          {caption}
        </div>
      ) : null}
    </motion.div>
  );
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`;
  return `${(n / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}

// Local mirror of Transcript's time formatter — this card is a
// self-contained block renderer (like PullRequestCard), so it keeps its own.
function hms(iso: string): string {
  return new Date(iso).toLocaleTimeString('en-GB', {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
  });
}
