import { motion } from 'framer-motion';
import { useEffect, useMemo, useRef, useState } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { createSession } from '../api';
import { useImages } from '../hooks/useImages';
import { useHarnesses } from '../hooks/useHarnesses';
import { SectionHead } from './HostManifest';
import type { HarnessSpec, ImageDescriptor, WorkspaceSpec } from '../types';

// "New session" form rendered inline on the Overview page, between the
// HostManifest and the SessionManifest. Phase 2 made the three session
// axes (image / workspace / harness) orthogonal, so the form has three
// first-class sections — none hidden behind disclosure. Image selection
// drives the harness dropdown (builtin names are scoped per-image) and
// the credentials section (secret schema is per-image).

type WorkspaceKind = 'empty' | 'git' | 'local_mount';
type HarnessKind = 'none' | 'builtin';

export interface NewSessionFormProps {
  onCancel: () => void;
  onCreated: (sessionId: string) => void;
}

export function NewSessionForm({ onCancel, onCreated }: NewSessionFormProps) {
  const { data: images, isLoading, error: loadError } = useImages(true);
  const { data: harnesses } = useHarnesses(true);
  const qc = useQueryClient();

  // ---- IMAGE ----
  const [selectedKey, setSelectedKey] = useState<string>('');
  const selected = useMemo<ImageDescriptor | undefined>(
    () => images?.find((img) => imageKey(img) === selectedKey),
    [images, selectedKey],
  );
  useEffect(() => {
    if (!selectedKey && images && images.length > 0) {
      setSelectedKey(imageKey(images[0]));
    }
  }, [images, selectedKey]);

  // ---- WORKSPACE ----
  const [workspaceKind, setWorkspaceKind] = useState<WorkspaceKind>('empty');
  const [gitUrl, setGitUrl] = useState('');
  const [gitBranch, setGitBranch] = useState('main');
  const [gitReadOnly, setGitReadOnly] = useState(false);
  const [hostPath, setHostPath] = useState('');
  const [guestPath, setGuestPath] = useState('/workspace');
  const [mountReadOnly, setMountReadOnly] = useState(false);

  // ---- HARNESS ----
  const [harnessKind, setHarnessKind] = useState<HarnessKind>('none');
  const [harnessName, setHarnessName] = useState<string>('');

  // Reset secrets when image changes — secret schema is per-image.
  // Harness selection is NOT reset — harnesses live above images now,
  // deployment-wide via the host's harness registry, so a session's
  // harness choice survives image swaps.
  useEffect(() => {
    setSecrets({});
    setError(null);
  }, [selectedKey]);

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
  const [secrets, setSecrets] = useState<Record<string, string>>({});
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const isBroker = selected?.secret_mode === 'broker';
  const requiredSecretsMissing =
    selected?.required_secrets
      .filter((s) => s.required)
      .some((s) => !(secrets[s.name] && secrets[s.name].length > 0)) ?? false;

  const supportsLocalMount = selected?.supports_local_mount ?? false;

  const workspaceValid =
    workspaceKind === 'empty' ||
    (workspaceKind === 'git' && gitUrl.trim().length > 0 && gitBranch.trim().length > 0) ||
    (workspaceKind === 'local_mount' &&
      hostPath.trim().length > 0 &&
      guestPath.trim().length > 0 &&
      supportsLocalMount);

  const canSubmit =
    !!selected &&
    workspaceValid &&
    !submitting &&
    !isBroker &&
    !requiredSecretsMissing;

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!canSubmit || !selected) return;
    setSubmitting(true);
    setError(null);
    try {
      const workspace: WorkspaceSpec =
        workspaceKind === 'empty'
          ? { kind: 'empty' }
          : workspaceKind === 'git'
            ? {
                kind: 'git',
                url: gitUrl.trim(),
                branch: gitBranch.trim(),
                read_only: gitReadOnly,
              }
            : {
                kind: 'local_mount',
                host_path: hostPath.trim(),
                guest_path: guestPath.trim(),
                read_only: mountReadOnly,
              };
      const harness: HarnessSpec =
        harnessKind === 'none' || !harnessName
          ? { kind: 'none' }
          : { kind: 'builtin', name: harnessName };
      const promptValue =
        harness.kind === 'none' ? undefined : prompt.trim() || undefined;

      const res = await createSession({
        image: { kind: 'registry', repo: selected.repo, tag: selected.tag },
        workspace,
        harness,
        prompt: promptValue,
        secrets: Object.keys(secrets).length > 0 ? secrets : undefined,
      });
      // Wipe sensitive form state immediately on success.
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
            no images registered. bake one with{' '}
            <code className="font-mono">just vz-bake-claude-oauth</code> —
            harnesses are now declared in <code className="font-mono">[[harness]]</code>{' '}
            blocks of <code className="font-mono">engram.toml</code>.
          </p>
        )}

        {images && images.length > 0 && (
          <>
            {/* IMAGE */}
            <div className="space-y-3">
              <SubHead>IMAGE</SubHead>
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
            </div>

            {/* WORKSPACE */}
            <div className="space-y-3">
              <SubHead>WORKSPACE</SubHead>
              <RadioStrip
                value={workspaceKind}
                onChange={(v) => setWorkspaceKind(v)}
                options={[
                  { value: 'empty', label: 'empty' },
                  { value: 'git', label: 'git' },
                  {
                    value: 'local_mount',
                    label: 'local mount',
                    disabled: !supportsLocalMount,
                    title: !supportsLocalMount
                      ? 'Local mount requires the VZ or Process backend. This deployment uses Firecracker.'
                      : undefined,
                  },
                ]}
              />
              {workspaceKind === 'git' && (
                <>
                  <Field label="url">
                    <input
                      type="text"
                      value={gitUrl}
                      onChange={(e) => setGitUrl(e.target.value)}
                      className="ledger-input font-mono"
                      placeholder="https://github.com/cortex/api.git"
                    />
                  </Field>
                  <Field label="branch">
                    <input
                      type="text"
                      value={gitBranch}
                      onChange={(e) => setGitBranch(e.target.value)}
                      className="ledger-input font-mono"
                      placeholder="main"
                    />
                  </Field>
                  <CheckRow
                    label="read only"
                    checked={gitReadOnly}
                    onChange={setGitReadOnly}
                  />
                </>
              )}
              {workspaceKind === 'local_mount' && (
                <>
                  <Field label="host path">
                    <input
                      type="text"
                      value={hostPath}
                      onChange={(e) => setHostPath(e.target.value)}
                      className="ledger-input font-mono"
                      placeholder="/Users/me/code"
                    />
                  </Field>
                  <Field label="guest path">
                    <input
                      type="text"
                      value={guestPath}
                      onChange={(e) => setGuestPath(e.target.value)}
                      className="ledger-input font-mono"
                      placeholder="/workspace"
                    />
                  </Field>
                  <CheckRow
                    label="read only"
                    checked={mountReadOnly}
                    onChange={setMountReadOnly}
                  />
                </>
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

            {/* CREDENTIALS */}
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

function CheckRow({
  label,
  checked,
  onChange,
}: {
  label: string;
  checked: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <label
      className="grid items-baseline gap-x-4"
      style={{ gridTemplateColumns: '7rem 1fr' }}
    >
      <span />
      <span className="inline-flex items-baseline gap-2 font-mono text-[0.82rem]">
        <input
          type="checkbox"
          checked={checked}
          onChange={(e) => onChange(e.target.checked)}
        />
        <span>{label}</span>
      </span>
    </label>
  );
}

function RadioStrip<T extends string>({
  value,
  onChange,
  options,
}: {
  value: T;
  onChange: (v: T) => void;
  options: {
    value: T;
    label: string;
    disabled?: boolean;
    title?: string;
  }[];
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
        source
      </span>
      <div className="flex gap-4">
        {options.map((opt) => {
          const selected = opt.value === value;
          return (
            <label
              key={opt.value}
              title={opt.title}
              className={`inline-flex items-baseline gap-1.5 font-mono text-[0.82rem] ${
                opt.disabled ? 'opacity-40 cursor-not-allowed' : 'cursor-pointer'
              }`}
            >
              <input
                type="radio"
                name="workspace_kind"
                checked={selected}
                disabled={opt.disabled}
                onChange={() => !opt.disabled && onChange(opt.value)}
              />
              <span>{opt.label}</span>
            </label>
          );
        })}
      </div>
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
