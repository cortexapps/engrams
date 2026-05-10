import { motion } from 'framer-motion';
import { useEffect, useRef, useState } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { createSession } from '../api';
import { useEnabledImages } from '../hooks/useEnabledImages';
import { useHarnesses } from '../hooks/useHarnesses';
import { SectionHead } from './HostManifest';
import type { HarnessSpec } from '../types';

// "New session" form rendered inline on the Overview page, between the
// HostManifest and the SessionManifest. ADR 0005 retired the
// workspace axis: the bake image's `/workspace` is the workspace.
// Two axes left — image + harness.

type HarnessKind = 'none' | 'builtin';

export interface NewSessionFormProps {
  onCancel: () => void;
  onCreated: (sessionId: string) => void;
}

export function NewSessionForm({ onCancel, onCreated }: NewSessionFormProps) {
  const { data: images, isLoading, error: loadError } = useEnabledImages(true);
  const { data: harnesses } = useHarnesses(true);
  const qc = useQueryClient();

  // ---- IMAGE ----
  // Stage D: image is a flat OCI URI. The picker shows the operator-
  // curated `enabled_images` set; whatever URI the user selects flows
  // straight to `POST /sessions { image: "<uri>" }`. Manifest data
  // (name, description) is read off the cached row so we don't refetch
  // the registry per render.
  const [selectedUri, setSelectedUri] = useState<string>('');
  const selected = images?.find((img) => img.image_uri === selectedUri);
  useEffect(() => {
    if (!selectedUri && images && images.length > 0) {
      setSelectedUri(images[0].image_uri);
    }
  }, [images, selectedUri]);

  // ---- HARNESS ----
  const [harnessKind, setHarnessKind] = useState<HarnessKind>('none');
  const [harnessName, setHarnessName] = useState<string>('');

  // Clear any prior submit error when the image changes — the
  // selection itself is the corrective action; we don't want a stale
  // "image X failed" still on screen for a different image. Harness
  // selection (and its credentials) is NOT reset — harnesses live
  // above images, deployment-wide via the registry-backed harness
  // packs, so a session's harness choice survives image swaps.
  useEffect(() => {
    setError(null);
  }, [selectedUri]);

  // If the host registry no longer offers the chosen harness (operator
  // removed a binary), drop back to none.
  useEffect(() => {
    if (
      harnessKind === 'builtin' &&
      harnesses &&
      !harnesses.some((h) => h.name === harnessName)
    ) {
      setHarnessKind('none');
      setHarnessName('');
    }
  }, [harnesses, harnessKind, harnessName]);

  // ---- PROMPT (only meaningful when harness != none) ----
  const [prompt, setPrompt] = useState('');

  // ---- CREDENTIALS ----
  // Stage D: the dashboard no longer surfaces image-declared secrets.
  // The manifest lives server-side on the `enabled_images` row; if an
  // image needs literal-mode secrets, they're supplied at the deploy
  // layer (env / KMS / Vault), not pasted in the browser. The form
  // still surfaces harness-level credentials — today only the Claude
  // OAuth/API key, which is genuinely a per-session human input.
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Claude auth UX (hard-coded; see comment in the JSX section).
  // The dropdown picks which env-var name the value gets sent under.
  const [claudeAuthMethod, setClaudeAuthMethod] = useState<'oauth' | 'api_key'>(
    'oauth',
  );
  const [claudeToken, setClaudeToken] = useState('');
  const claudeTokenName =
    claudeAuthMethod === 'oauth'
      ? 'CLAUDE_CODE_OAUTH_TOKEN'
      : 'ANTHROPIC_API_KEY';

  // Claude harness needs *some* token to authenticate; if the user
  // picked it but didn't paste one, can't submit.
  const claudeTokenMissing =
    harnessKind === 'builtin' &&
    harnessName === 'claude' &&
    claudeToken.trim().length === 0;

  const canSubmit = !!selected && !submitting && !claudeTokenMissing;

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!canSubmit || !selected) return;
    setSubmitting(true);
    setError(null);
    try {
      const harness: HarnessSpec =
        harnessKind === 'none' || !harnessName
          ? { kind: 'none' }
          : { kind: 'builtin', name: harnessName };
      const promptValue =
        harness.kind === 'none' ? undefined : prompt.trim() || undefined;

      // Harness-level credentials (today: just the claude token, one
      // of OAuth or API key per `claudeAuthMethod`). Image-declared
      // secrets flow through the deploy layer post-Stage-D, not the
      // dashboard.
      const mergedSecrets: Record<string, string> = {};
      if (harnessKind === 'builtin' && harnessName === 'claude') {
        mergedSecrets[claudeTokenName] = claudeToken;
      }
      const res = await createSession({
        image: selected.image_uri,
        harness,
        prompt: promptValue,
        secrets:
          Object.keys(mergedSecrets).length > 0 ? mergedSecrets : undefined,
      });
      // Wipe sensitive form state immediately on success.
      setClaudeToken('');
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

      <form onSubmit={submit} className="space-y-6">
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
            no images enabled. push one with{' '}
            <code className="font-mono">engram image build --push</code>, then{' '}
            <a
              href="/settings/images"
              className="font-display italic"
              style={{ color: 'var(--color-amber)' }}
            >
              enable
            </a>{' '}
            it from settings.
          </p>
        )}

        {images && images.length > 0 && (
          <>
            {/* IMAGE */}
            <div className="space-y-3">
              <SubHead>IMAGE</SubHead>
              <Field label="image">
                <select
                  value={selectedUri}
                  onChange={(e) => setSelectedUri(e.target.value)}
                  className="ledger-input font-display"
                >
                  {images.map((img) => (
                    <option key={img.image_uri} value={img.image_uri}>
                      {img.image_uri}
                      {img.manifest_name ? ` — ${img.manifest_name}` : ''}
                    </option>
                  ))}
                </select>
              </Field>
              {selected?.manifest_description && (
                <p
                  className="font-display italic text-[0.85rem] -mt-1"
                  style={{ color: 'var(--color-ink-quiet)' }}
                >
                  {selected.manifest_description}
                </p>
              )}
            </div>

            {/* HARNESS */}
            <div className="space-y-3">
              <SubHead>HARNESS</SubHead>
              <Field label="agent">
                <select
                  value={harnessKind === 'none' ? '__none__' : harnessName}
                  onChange={(e) => {
                    const v = e.target.value;
                    if (v === '__none__') {
                      setHarnessKind('none');
                      setHarnessName('');
                    } else {
                      setHarnessKind('builtin');
                      setHarnessName(v);
                    }
                  }}
                  className="ledger-input font-display"
                >
                  <option value="__none__">none</option>
                  {harnesses?.map((h) => (
                    <option key={h.name} value={h.name}>
                      builtin: {h.name}
                      {h.description ? ` — ${h.description}` : ''}
                    </option>
                  ))}
                </select>
              </Field>
              {harnesses && harnesses.length === 0 && (
                <p
                  className="font-display italic text-[0.85rem] -mt-1"
                  style={{ color: 'var(--color-ink-quiet)' }}
                >
                  no harnesses registered on this host; sessions run as plain shells
                </p>
              )}
              {harnessKind === 'builtin' && (
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
              )}
            </div>

            {/* HARNESS-LEVEL CREDENTIALS — hard-coded UX for the
                `claude` harness today. Long-term these should come
                from the harness pack itself, but for v1 we have one
                special case: claude needs either an OAuth token
                (long-lived, from `claude setup-token`) or an API
                key (sk-ant-...). The dropdown picks the env-var
                name; whichever's chosen lands as a literal env var
                injected into the harness process. */}
            {harnessKind === 'builtin' && harnessName === 'claude' && (
              <div className="pt-2 space-y-3">
                <p
                  className="font-mono smallcaps text-[0.65rem]"
                  style={{ color: 'var(--color-ink-quiet)' }}
                >
                  CLAUDE CREDENTIALS · paste once · never persisted
                </p>
                <Field label="auth method">
                  <select
                    value={claudeAuthMethod}
                    onChange={(e) => {
                      setClaudeAuthMethod(
                        e.target.value as 'oauth' | 'api_key',
                      );
                      // Wipe the previous-method's value so we
                      // don't ship a stale token under the wrong
                      // env-var name.
                      setClaudeToken('');
                    }}
                    className="ledger-input font-display"
                  >
                    <option value="oauth">
                      OAuth token — `claude setup-token` (sk-ant-oat01-…)
                    </option>
                    <option value="api_key">
                      API key — sk-ant-api03-…
                    </option>
                  </select>
                </Field>
                <SecretField
                  name={claudeTokenName}
                  required={true}
                  value={claudeToken}
                  onChange={setClaudeToken}
                />
              </div>
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

function SubHead({ children }: { children: React.ReactNode }) {
  return (
    <p
      className="font-mono smallcaps text-[0.65rem]"
      style={{ color: 'var(--color-ink-quiet)' }}
    >
      {children}
    </p>
  );
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
