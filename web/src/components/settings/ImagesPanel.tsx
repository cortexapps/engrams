import { AnimatePresence, motion } from 'framer-motion';
import { useState } from 'react';
import {
  useDisableImage,
  useEnableImage,
  useEnabledImages,
  useRefreshEnabledImage,
} from '../../hooks/useEnabledImages';
import {
  isJobActive,
  useEnableJobs,
  useRetryEnableJob,
} from '../../hooks/useEnableJobs';
import type { EnableJob, EnabledImageSummary } from '../../types';
import { Field, FormError, PressButton, SubHead } from './_form';

// Enabled-images panel — Stage D + ADR 0036. Operators curate the OCI
// URIs sessions may reference. The list reads `GET /api/enabled-images`
// (Postgres-backed); enabling POSTs a job (202) and the panel polls
// `GET /api/enable-jobs` to render REAL pipeline progress
// (materializing chunks → capturing snapshot → ready).
//
// Layout mirrors RegistriesPanel exactly:
//   • A list of rows, one per enabled URI. Each row shows a status
//     glyph, the URI, the parsed manifest name, the digest, and
//     refresh + disable controls.
//   • Inline expand "+ enable a new image" form with one input.
//   • Empty state with an italic prompt.

export function ImagesPanel() {
  const { data, isLoading, error } = useEnabledImages();
  const { data: jobs } = useEnableJobs();
  const [addOpen, setAddOpen] = useState(false);

  // ADR 0036: in-flight enables (and fresh failures, kept visible
  // for an hour so the error + retry affordance doesn't vanish).
  const visibleJobs = (jobs ?? []).filter(
    (j) =>
      isJobActive(j) ||
      (j.state === 'failed' &&
        Date.now() - new Date(j.updated_at).getTime() < 60 * 60 * 1000),
  );

  return (
    <section>
      <SectionHeader />

      {visibleJobs.length > 0 && (
        <ul className="space-y-0 mb-6">
          {visibleJobs.map((job) => (
            <EnableJobRow key={job.id} job={job} />
          ))}
        </ul>
      )}

      {error && (
        <p
          className="font-display italic text-[0.9rem] mb-4"
          style={{ color: 'var(--color-amber)' }}
        >
          could not load enabled images — {String(error)}
        </p>
      )}

      {isLoading && <ListLoadingSkeleton />}

      {!isLoading && data && data.length > 0 && (
        <ul className="space-y-0">
          {data.map((row) => (
            <ImageRow key={row.id} row={row} />
          ))}
        </ul>
      )}

      {!isLoading && data && data.length === 0 && !addOpen && <EmptyState />}

      <AnimatePresence initial={false}>
        {addOpen && (
          <motion.div
            key="add-form"
            layout
            initial={{ opacity: 0, height: 0 }}
            animate={{ opacity: 1, height: 'auto' }}
            exit={{ opacity: 0, height: 0 }}
            transition={{ duration: 0.25, ease: 'easeOut' }}
            style={{ overflow: 'hidden' }}
            className="mt-8"
          >
            <EnableImageForm
              onCancel={() => setAddOpen(false)}
              onAdded={() => setAddOpen(false)}
            />
          </motion.div>
        )}
      </AnimatePresence>

      {!addOpen && (
        <div className="mt-8 flex justify-center">
          <PressButton onClick={() => setAddOpen(true)} tone="primary">
            + enable a new image
          </PressButton>
        </div>
      )}
    </section>
  );
}

function SectionHeader() {
  return (
    <header className="mb-6 flex items-baseline justify-between">
      <h2
        className="font-mono smallcaps text-[0.7rem]"
        style={{ color: 'var(--color-ink-quiet)', letterSpacing: '0.18em' }}
      >
        Enabled Images
      </h2>
      <p
        className="font-display italic text-[0.8rem]"
        style={{ color: 'var(--color-ink-quiet)' }}
      >
        manifest cached on enable, refresh when tags move
      </p>
    </header>
  );
}

function ListLoadingSkeleton() {
  return (
    <p
      className="font-display italic text-[0.9rem] py-3"
      style={{ color: 'var(--color-ink-quiet)' }}
    >
      loading…
    </p>
  );
}

function EmptyState() {
  return (
    <div className="py-10 text-center" style={{ minHeight: '8rem' }}>
      <p
        className="font-display italic text-[1.05rem]"
        style={{ color: 'var(--color-ink-faded)' }}
      >
        no images enabled.
      </p>
      <p
        className="font-display italic text-[0.9rem] mt-2"
        style={{ color: 'var(--color-ink-quiet)' }}
      >
        bake + push an image with{' '}
        <code className="font-mono">engram image build --push</code>, then
        enable
        <br />
        the URI here so sessions can reference it.
      </p>
    </div>
  );
}

// ---------- Row -----------------------------------------------------

function ImageRow({ row }: { row: EnabledImageSummary }) {
  const [confirming, setConfirming] = useState(false);
  const del = useDisableImage();
  const refresh = useRefreshEnabledImage();

  return (
    <motion.li
      layout
      initial={{ opacity: 0, y: 4 }}
      animate={{ opacity: 1, y: 0 }}
      exit={{ opacity: 0 }}
      transition={{ duration: 0.25, ease: 'easeOut' }}
      className="py-4"
      style={{ borderBottom: '1px solid var(--color-rule-faint)' }}
    >
      <div className="flex items-baseline gap-3 flex-wrap">
        <span
          aria-hidden
          className="glyph"
          style={{ color: 'var(--color-ink-faded)' }}
        >
          ●
        </span>
        <span
          className="font-mono"
          style={{
            fontSize: '0.95rem',
            color: 'var(--color-ink)',
            whiteSpace: 'nowrap',
          }}
        >
          {row.image_uri}
        </span>
        {row.manifest_name && (
          <span
            className="font-display italic"
            style={{ fontSize: '0.85rem', color: 'var(--color-ink-faded)' }}
          >
            {row.manifest_name}
          </span>
        )}
        <DigestChip digest={row.manifest_digest} />
        <span className="ml-auto flex items-baseline gap-4">
          <span
            className="font-mono text-[0.72rem]"
            style={{ color: 'var(--color-ink-quiet)' }}
            title={new Date(row.last_refreshed_at).toLocaleString()}
          >
            refreshed {timeAgo(row.last_refreshed_at)}
          </span>
          <PressButton
            onClick={() => refresh.mutate(row.image_uri)}
            disabled={refresh.isPending}
          >
            {refresh.isPending ? 'refreshing…' : 'refresh'}
          </PressButton>
          {!confirming ? (
            <PressButton onClick={() => setConfirming(true)}>
              disable
            </PressButton>
          ) : (
            <span className="flex items-baseline gap-3">
              <span
                className="font-display italic"
                style={{
                  color: 'var(--color-ink-faded)',
                  fontSize: '0.85rem',
                }}
              >
                sure?
              </span>
              <PressButton
                onClick={() => del.mutate(row.image_uri)}
                tone="danger"
                disabled={del.isPending}
              >
                {del.isPending ? 'disabling…' : 'yes'}
              </PressButton>
              <PressButton onClick={() => setConfirming(false)}>no</PressButton>
            </span>
          )}
        </span>
      </div>
      {row.manifest_description && (
        <p
          className="font-display italic text-[0.82rem] mt-1"
          style={{ color: 'var(--color-ink-quiet)', marginLeft: '1.4rem' }}
        >
          {row.manifest_description}
        </p>
      )}
      {refresh.error && (
        <p
          className="font-display italic text-[0.85rem] mt-2"
          style={{ color: 'var(--color-amber)', marginLeft: '1.4rem' }}
        >
          could not refresh — {String(refresh.error)}
        </p>
      )}
      {del.error && (
        <p
          className="font-display italic text-[0.85rem] mt-2"
          style={{ color: 'var(--color-amber)', marginLeft: '1.4rem' }}
        >
          could not disable — {String(del.error)}
        </p>
      )}
    </motion.li>
  );
}

/** sha256:abcd1234… digest, abbreviated to the prefix and styled as
 * a chip. The full digest sits in `title=` for hover. */
function DigestChip({ digest }: { digest: string }) {
  const short = digest.length > 19 ? `${digest.slice(0, 19)}…` : digest;
  return (
    <span
      className="font-mono"
      title={digest}
      style={{
        fontSize: '0.65rem',
        color: 'var(--color-ink-faded)',
        border: '1px solid var(--color-rule)',
        padding: '0.1rem 0.5rem',
        letterSpacing: '0.04em',
      }}
    >
      {short}
    </span>
  );
}

// ---------- Enable form ---------------------------------------------

function EnableImageForm({
  onCancel,
  onAdded,
}: {
  onCancel: () => void;
  onAdded: () => void;
}) {
  const [imageUri, setImageUri] = useState('');
  const enable = useEnableImage();
  const [submitError, setSubmitError] = useState<string | null>(null);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setSubmitError(null);
    if (!imageUri.trim()) {
      setSubmitError('image URI is required');
      return;
    }
    try {
      await enable.mutateAsync(imageUri.trim());
      setImageUri('');
      onAdded();
    } catch (err) {
      setSubmitError(String(err));
    }
  };

  return (
    <form
      onSubmit={submit}
      className="space-y-6 pt-2 pb-4"
      style={{
        borderTop: '1px solid var(--color-rule)',
        borderBottom: '1px solid var(--color-rule)',
        paddingTop: '1.5rem',
      }}
    >
      <SubHead>NEW ENABLED IMAGE</SubHead>

      <Field
        label="image uri"
        hint="full OCI reference: <host>[:port]/<repo>:<tag>. The coordinator pulls the manifest layer from this URI on enable; subsequent session-create reads from the cached row."
      >
        <input
          type="text"
          value={imageUri}
          autoFocus
          onChange={(e) => setImageUri(e.target.value)}
          className="ledger-input font-mono"
          placeholder="ghcr.io/cortex/api:warm-1"
          spellCheck={false}
          autoCapitalize="off"
        />
      </Field>

      {submitError && <FormError message={submitError} />}

      <div className="flex items-baseline gap-6 pt-2">
        <PressButton type="submit" tone="primary" disabled={enable.isPending}>
          {enable.isPending ? 'validating…' : 'enable'}
        </PressButton>
        <PressButton onClick={onCancel} disabled={enable.isPending}>
          cancel
        </PressButton>
      </div>
    </form>
  );
}

// ---------- Enable-job progress (ADR 0036) ---------------------------
//
// Real progress from the server: the coordinator's scanner drives the
// job through pending → materializing → capturing → ready, updating
// chunks_done/chunks_total as it materializes. We render a thin bar +
// the state label; failed jobs keep their error visible with a retry
// affordance.

const JOB_STATE_LABEL: Record<EnableJob['state'], string> = {
  pending: 'queued',
  materializing: 'materializing chunks',
  capturing: 'capturing canonical snapshot',
  ready: 'ready',
  failed: 'failed',
};

function EnableJobRow({ job }: { job: EnableJob }) {
  const retry = useRetryEnableJob();
  const failed = job.state === 'failed';
  const pct =
    job.chunks_total && job.chunks_total > 0
      ? Math.min(100, Math.round((job.chunks_done / job.chunks_total) * 100))
      : null;

  return (
    <motion.li
      layout
      initial={{ opacity: 0, y: 4 }}
      animate={{ opacity: 1, y: 0 }}
      exit={{ opacity: 0 }}
      transition={{ duration: 0.25, ease: 'easeOut' }}
      className="py-3"
      style={{ borderBottom: '1px solid var(--color-rule-faint)' }}
    >
      <div className="flex items-baseline gap-3 flex-wrap">
        <span
          aria-hidden
          className="glyph"
          style={{ color: failed ? 'var(--color-amber)' : 'var(--color-ink-faded)' }}
        >
          {failed ? '✕' : '◌'}
        </span>
        <span
          className="font-mono"
          style={{
            fontSize: '0.95rem',
            color: 'var(--color-ink)',
            whiteSpace: 'nowrap',
          }}
        >
          {job.image_uri}
        </span>
        <span
          className="font-mono text-xs smallcaps inline-flex items-baseline gap-1"
          style={{ color: failed ? 'var(--color-amber)' : 'var(--color-ink-quiet)' }}
        >
          {JOB_STATE_LABEL[job.state]}
          {job.state === 'materializing' && job.chunks_total ? (
            <span>
              · {job.chunks_done}/{job.chunks_total} chunks
            </span>
          ) : null}
          {!failed && <DotPulse />}
        </span>
        {failed && (
          <span className="ml-auto">
            <PressButton
              onClick={() => retry.mutate(job.id)}
              disabled={retry.isPending}
            >
              {retry.isPending ? 'retrying…' : 'retry'}
            </PressButton>
          </span>
        )}
      </div>
      {pct !== null && !failed && (
        <div
          className="mt-2"
          style={{
            marginLeft: '1.4rem',
            height: '3px',
            background: 'var(--color-rule-faint)',
            maxWidth: '28rem',
          }}
        >
          <motion.div
            animate={{ width: `${pct}%` }}
            transition={{ duration: 0.4, ease: 'easeOut' }}
            style={{ height: '100%', background: 'var(--color-ink-faded)' }}
          />
        </div>
      )}
      {failed && job.error && (
        <p
          className="font-display italic text-[0.85rem] mt-2"
          style={{ color: 'var(--color-amber)', marginLeft: '1.4rem' }}
        >
          {job.error}
        </p>
      )}
    </motion.li>
  );
}

// Three dots that fade-pulse in sequence. Cheap, lightweight,
// matches the ledger aesthetic — no spinning gradients or
// material wheels.
function DotPulse() {
  return (
    <span className="inline-flex items-baseline" aria-hidden>
      {[0, 1, 2].map((i) => (
        <motion.span
          key={i}
          initial={{ opacity: 0.2 }}
          animate={{ opacity: [0.2, 1, 0.2] }}
          transition={{
            duration: 1.2,
            repeat: Infinity,
            delay: i * 0.18,
            ease: 'easeInOut',
          }}
          style={{ display: 'inline-block', marginLeft: '0.15ch' }}
        >
          .
        </motion.span>
      ))}
    </span>
  );
}

// ---------- helpers -------------------------------------------------

function timeAgo(iso: string): string {
  const then = new Date(iso).getTime();
  const now = Date.now();
  if (Number.isNaN(then)) return iso;
  const seconds = Math.max(0, Math.floor((now - then) / 1000));
  if (seconds < 60) return 'just now';
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 48) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days}d ago`;
  return new Date(iso).toLocaleDateString();
}
