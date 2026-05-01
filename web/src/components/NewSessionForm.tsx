import { motion } from 'framer-motion';
import { useEffect, useMemo, useRef, useState } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { createSession } from '../api';
import { useImages } from '../hooks/useImages';
import { SectionHead } from './HostManifest';
import type { ImageDescriptor } from '../types';

// "New session" form rendered inline on the Overview page, between the
// HostManifest and the SessionManifest. Image-driven: when the user
// picks an image, the form re-renders one masked input per required
// secret so the credential ask matches the image's manifest. Tokens
// live in React state for the form's lifetime and are cleared on
// unmount — matches the "no users yet" framing.

export interface NewSessionFormProps {
  onCancel: () => void;
  onCreated: (sessionId: string) => void;
}

export function NewSessionForm({ onCancel, onCreated }: NewSessionFormProps) {
  const { data: images, isLoading, error: loadError } = useImages(true);
  const qc = useQueryClient();

  // The dropdown's selected `(repo, tag)` pair.
  const [selectedKey, setSelectedKey] = useState<string>('');
  const selected = useMemo<ImageDescriptor | undefined>(
    () => images?.find((img) => imageKey(img) === selectedKey),
    [images, selectedKey],
  );

  // Default to the first available image once we load.
  useEffect(() => {
    if (!selectedKey && images && images.length > 0) {
      setSelectedKey(imageKey(images[0]));
    }
  }, [images, selectedKey]);

  const [branch, setBranch] = useState('main');
  const [prompt, setPrompt] = useState('');
  const [secrets, setSecrets] = useState<Record<string, string>>({});
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Reset secrets whenever the image changes — different images declare
  // different env vars, and we must NOT silently retain a token that
  // belonged to a different credential.
  useEffect(() => {
    setSecrets({});
    setError(null);
  }, [selectedKey]);

  const isBroker = selected?.secret_mode === 'broker';
  const requiredSecretsMissing =
    selected?.required_secrets
      .filter((s) => s.required)
      .some((s) => !(secrets[s.name] && secrets[s.name].length > 0)) ?? false;

  const canSubmit =
    !!selected && !!branch && !submitting && !isBroker && !requiredSecretsMissing;

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!canSubmit || !selected) return;
    setSubmitting(true);
    setError(null);
    try {
      const res = await createSession({
        repo: selected.repo,
        branch,
        image_version: selected.tag,
        prompt: prompt.trim() ? prompt : undefined,
        secrets: Object.keys(secrets).length > 0 ? secrets : undefined,
      });
      // Clear sensitive form state immediately on success — no token
      // lingers in React memory longer than the network round trip.
      setSecrets({});
      setPrompt('');
      qc.invalidateQueries({ queryKey: ['sessions'] });
      onCreated(res.session_id);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <motion.section
      layout
      initial={{ opacity: 0, y: -6 }}
      animate={{ opacity: 1, y: 0 }}
      exit={{ opacity: 0, y: -6 }}
      transition={{ duration: 0.35, ease: 'easeOut' }}
      className="mb-12"
    >
      <div className="flex items-baseline justify-between mb-4">
        <SectionHead label="NEW SESSION" />
        <button
          type="button"
          onClick={onCancel}
          className="font-mono smallcaps text-[0.7rem] -mt-1 hover:[color:var(--color-ink)] transition-colors"
          style={{ color: 'var(--color-ink-quiet)' }}
        >
          × close
        </button>
      </div>

      <form onSubmit={submit} className="space-y-3">
        {isLoading && (
          <p
            className="font-display italic text-[0.85rem]"
            style={{ color: 'var(--color-ink-quiet)' }}
          >
            loading images…
          </p>
        )}
        {loadError && (
          <p
            className="font-display italic text-[0.85rem]"
            style={{ color: 'var(--color-ink-faded)' }}
          >
            failed to load images: {String(loadError)}
          </p>
        )}
        {!isLoading && images && images.length === 0 && (
          <p
            className="font-display italic text-[0.85rem]"
            style={{ color: 'var(--color-ink-quiet)' }}
          >
            no images registered. bake one with{' '}
            <code className="font-mono">just vz-bake-claude-oauth</code>.
          </p>
        )}

        {images && images.length > 0 && (
          <>
            <Field label="image">
              <select
                value={selectedKey}
                onChange={(e) => setSelectedKey(e.target.value)}
                className="ledger-input font-display"
              >
                {images.map((img) => (
                  <option key={imageKey(img)} value={imageKey(img)}>
                    {img.repo} · {img.tag}
                  </option>
                ))}
              </select>
            </Field>

            {selected?.description && (
              <p
                className="font-display italic text-[0.85rem] -mt-1"
                style={{ color: 'var(--color-ink-quiet)' }}
              >
                {selected.description}
              </p>
            )}

            <Field label="branch">
              <input
                type="text"
                value={branch}
                onChange={(e) => setBranch(e.target.value)}
                className="ledger-input font-mono"
                placeholder="main"
              />
            </Field>

            <Field label="prompt">
              <textarea
                value={prompt}
                onChange={(e) => setPrompt(e.target.value)}
                rows={2}
                className="ledger-input font-display"
                placeholder="optional opening prompt"
                style={{ resize: 'vertical' }}
              />
            </Field>

            {selected && selected.required_secrets.length > 0 && (
              <div className="pt-2 space-y-3">
                <p
                  className="font-mono smallcaps text-[0.65rem]"
                  style={{ color: 'var(--color-ink-quiet)' }}
                >
                  CREDENTIALS · paste once · never persisted
                </p>
                {selected.required_secrets.map((s) => (
                  <SecretField
                    key={s.name}
                    name={s.name}
                    required={s.required}
                    value={secrets[s.name] ?? ''}
                    onChange={(v) =>
                      setSecrets((prev) => ({ ...prev, [s.name]: v }))
                    }
                    disabled={isBroker}
                  />
                ))}
              </div>
            )}

            {isBroker && (
              <p
                className="font-display italic text-[0.85rem]"
                style={{ color: 'var(--color-ink-faded)' }}
              >
                this image uses the broker secret mode — credentials must be
                provisioned host-side, not pasted here.
              </p>
            )}

            {error && (
              <p
                className="font-mono text-[0.78rem]"
                style={{ color: 'var(--color-amber)' }}
              >
                {error}
              </p>
            )}

            <div className="flex items-center justify-end pt-2">
              <button
                type="submit"
                disabled={!canSubmit}
                className={`font-mono smallcaps text-[0.78rem] tracking-[0.12em] transition-colors ${
                  canSubmit ? '' : 'opacity-40 cursor-not-allowed'
                }`}
                style={{
                  color: canSubmit
                    ? 'var(--color-amber)'
                    : 'var(--color-ink-quiet)',
                }}
              >
                {submitting ? 'starting…' : 'start →'}
              </button>
            </div>
          </>
        )}
      </form>
      <hr className="mt-6" />
    </motion.section>
  );
}

function imageKey(img: ImageDescriptor): string {
  return `${img.repo}::${img.tag}`;
}

function Field({
  label,
  children,
}: {
  label: string;
  children: React.ReactNode;
}) {
  return (
    <label
      className="grid items-baseline gap-x-4"
      style={{ gridTemplateColumns: '7rem 1fr' }}
    >
      <span
        className="font-mono smallcaps text-[0.7rem]"
        style={{ color: 'var(--color-ink-quiet)' }}
      >
        {label}
      </span>
      {children}
    </label>
  );
}

function SecretField({
  name,
  required,
  value,
  onChange,
  disabled,
}: {
  name: string;
  required: boolean;
  value: string;
  onChange: (v: string) => void;
  disabled?: boolean;
}) {
  const ref = useRef<HTMLInputElement>(null);
  return (
    <label
      className="grid items-baseline gap-x-4"
      style={{ gridTemplateColumns: '17rem 1fr' }}
    >
      <span
        className="font-mono text-[0.78rem]"
        style={{ color: 'var(--color-ink-faded)' }}
        title={required ? 'required' : 'optional'}
      >
        {name}
      </span>
      <input
        ref={ref}
        type="password"
        autoComplete="off"
        spellCheck={false}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        disabled={disabled}
        className="ledger-input font-mono"
        placeholder={required ? '••••••••' : 'optional'}
      />
    </label>
  );
}
