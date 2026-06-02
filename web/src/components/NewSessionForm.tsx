import { motion } from 'framer-motion';
import { useEffect, useRef, useState } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { createSession } from '../api';
import { useEnabledImages } from '../hooks/useEnabledImages';
import { SectionHead } from './SectionHead';
import type { SessionMode } from '../types';

// "New session" form, revealed inline on the Sessions surface when the
// `+ new session` action is clicked. ADR 0005 retired the workspace
// axis; ADR 0021 P1.3 retired the per-session harness *selection* —
// the harness is baked into the image at image-bake time. Two axes
// left: image + mode.

export interface NewSessionFormProps {
  onCancel: () => void;
  onCreated: (sessionId: string) => void;
}

export function NewSessionForm({ onCancel, onCreated }: NewSessionFormProps) {
  const { data: images, isLoading, error: loadError } = useEnabledImages(true);
  const qc = useQueryClient();

  // ---- IMAGE ----
  // Stage D: image is a flat OCI URI. The picker shows the operator-
  // curated `enabled_images` set; whatever URI the user selects flows
  // straight to `POST /sessions { image: "<uri>" }`. The manifest's
  // `harness_name` (lifted server-side from `[harness] name = ...`)
  // tells us whether the image has a baked agent and which one.
  const [selectedUri, setSelectedUri] = useState<string>('');
  const selected = images?.find((img) => img.image_uri === selectedUri);
  useEffect(() => {
    if (!selectedUri && images && images.length > 0) {
      setSelectedUri(images[0].image_uri);
    }
  }, [images, selectedUri]);

  // ---- MODE ----
  // `agent`: drive the image's baked harness (default).
  // `dev_vm`: shell-only — even on a harnessed image, leave the agent
  //            resident-but-undriven.
  // For a harness-less image both modes look the same (no harness to
  // drive); we still send `mode` for wire uniformity.
  const [mode, setMode] = useState<SessionMode>('agent');

  // Image-driven derived state.
  const harnessName = selected?.harness_name ?? null;
  const hasHarness = harnessName !== null;
  const isClaude = harnessName === 'claude';
  // Prompt is meaningful only when the session will actually drive
  // an agent. dev-VM mode + harness-less images both make prompt
  // a no-op.
  const promptMeaningful = hasHarness && mode === 'agent';

  // ---- PROMPT ----
  const [prompt, setPrompt] = useState('');

  // ---- CREDENTIALS ----
  // ADR 0021 P1.3+: the dashboard surfaces per-session credentials
  // only when the image actually has the harness that needs them.
  // Today that's only the Claude harness (`harness_name = "claude"`),
  // which needs either an OAuth token (long-lived, from `claude
  // setup-token`) or an API key (sk-ant-...).
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [claudeAuthMethod, setClaudeAuthMethod] = useState<'oauth' | 'api_key'>(
    'oauth',
  );
  const [claudeToken, setClaudeToken] = useState('');
  const claudeTokenName =
    claudeAuthMethod === 'oauth'
      ? 'CLAUDE_CODE_OAUTH_TOKEN'
      : 'ANTHROPIC_API_KEY';

  // Claude needs *some* token to authenticate; the picker only
  // surfaces (and only blocks submit) on Claude images in agent mode.
  const claudeTokenMissing =
    isClaude && mode === 'agent' && claudeToken.trim().length === 0;

  const canSubmit = !!selected && !submitting && !claudeTokenMissing;

  // Wipe any prior submit error when the image changes — the
  // selection itself is the corrective action.
  useEffect(() => {
    setError(null);
  }, [selectedUri]);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!canSubmit || !selected) return;
    setSubmitting(true);
    setError(null);
    try {
      const promptValue = promptMeaningful ? prompt.trim() || undefined : undefined;

      // Harness-level credentials surface only for the image's actual
      // baked harness. Today: just Claude (one of OAuth or API key
      // per `claudeAuthMethod`). Image-declared `[secrets.*]` flow
      // through the deploy layer post-Stage-D, not the dashboard.
      const mergedSecrets: Record<string, string> = {};
      if (isClaude && mode === 'agent') {
        mergedSecrets[claudeTokenName] = claudeToken;
      }
      const res = await createSession({
        image: selected.image_uri,
        // Omit `mode` on the default (`agent`) so the wire stays
        // minimal; coord defaults to Agent server-side.
        mode: mode === 'dev_vm' ? 'dev_vm' : undefined,
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
              <p
                className="font-display italic text-[0.85rem] -mt-1"
                style={{ color: 'var(--color-ink-quiet)' }}
              >
                {harnessName
                  ? `baked harness: ${harnessName}`
                  : 'no baked harness — shell-only image'}
              </p>
            </div>

            {/* MODE */}
            <div className="space-y-3">
              <SubHead>MODE</SubHead>
              <Field label="mode">
                <select
                  value={mode}
                  onChange={(e) => setMode(e.target.value as SessionMode)}
                  className="ledger-input font-display"
                >
                  <option value="agent">
                    agent — drive the image's baked harness
                  </option>
                  <option value="dev_vm">
                    dev VM — shell-only, harness (if any) stays undriven
                  </option>
                </select>
              </Field>
              {promptMeaningful && (
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
                claude harness. Surfaces only when the *image* has a
                baked claude harness AND mode is agent (dev-VM
                doesn't drive the harness, so the credential is
                inert). Long-term these credential schemas should
                live alongside the built-in harness artifact and the
                form learns them from there. */}
            {isClaude && mode === 'agent' && (
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
