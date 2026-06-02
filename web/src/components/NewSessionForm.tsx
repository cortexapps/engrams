import { motion } from 'framer-motion';
import { useEffect, useState } from 'react';
import { Link } from 'react-router-dom';
import { useQueryClient } from '@tanstack/react-query';
import { createSession } from '../api';
import { useAuth } from '../auth/AuthProvider';
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

  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // ---- CREDENTIALS (ADR 0031) ----
  // We never prompt for the Claude token per session. The user saves it once
  // on their profile and the coordinator auto-injects it for built-in-Claude
  // sessions. If they haven't saved one yet, route them to the token screen
  // instead of letting them create a session that would fail to authenticate.
  const { principal } = useAuth();
  const needsToken = isClaude && mode === 'agent' && !principal.has_claude_token;

  const canSubmit = !!selected && !submitting && !needsToken;

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

      // ADR 0031: no per-session secrets. The Claude token is auto-injected
      // server-side from the user's profile for built-in-Claude sessions.
      const res = await createSession({
        image: selected.image_uri,
        // Omit `mode` on the default (`agent`) so the wire stays
        // minimal; coord defaults to Agent server-side.
        mode: mode === 'dev_vm' ? 'dev_vm' : undefined,
        prompt: promptValue,
      });
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

            {/* ADR 0031: a built-in-Claude session uses the token saved on
                your profile — no per-session prompt. If none is saved, route
                to the token screen instead of letting create fail. */}
            {needsToken && (
              <p
                className="font-display italic text-[0.85rem]"
                style={{ color: 'var(--color-ink-quiet)' }}
              >
                this image runs built-in Claude, which uses your saved Claude
                Code token — you don’t have one yet.
              </p>
            )}
            {isClaude && mode === 'agent' && !needsToken && (
              <p
                className="font-display italic text-[0.85rem]"
                style={{ color: 'var(--color-ink-quiet)' }}
              >
                built-in Claude — your saved token is used automatically.
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
              {needsToken ? (
                <Link
                  to="/settings/tokens"
                  className="font-mono smallcaps text-[0.78rem] tracking-[0.12em] transition-colors"
                  style={{ color: 'var(--color-amber)' }}
                >
                  save your Claude token to start →
                </Link>
              ) : (
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
              )}
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

