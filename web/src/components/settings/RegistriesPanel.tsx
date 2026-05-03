import { AnimatePresence, motion } from 'framer-motion';
import { useState } from 'react';
import {
  useAddRegistry,
  useDeleteRegistry,
  useRegistries,
} from '../../hooks/useRegistries';
import type {
  AddRegistryAuth,
  RegistryAuthKind,
  RegistryCredentialSummary,
} from '../../types';
import { Field, FormError, PressButton, SubHead } from './_form';

// Registries panel — the centerpiece of the settings surface. Lists
// configured Docker registries, lets operators add new ones with
// either a static credential (sealed under the deployment KEK) or
// the ambient GCP Workload Identity. A third option (AWS instance
// role) is rendered as "coming soon" so the UI matrix matches the
// `RegistryAuthSpec` enum's design intent — there's no schema
// migration when AWS lands; just a new auth-kind branch.
//
// Layout:
//   • A list of rows, one per registered host. Each row shows a
//     status glyph, the host, an auth-kind chip, the principal,
//     and a delete control.
//   • Below the list, an italic "+ register a new" prompt that
//     expands inline into the add form (mirroring NewSessionForm's
//     pattern — the settings page never navigates to a separate
//     URL for "add").
//   • An empty state with an inviting italic prompt rather than a
//     bleak "no records" placeholder.

export function RegistriesPanel() {
  const { data, isLoading, error } = useRegistries();
  const [addOpen, setAddOpen] = useState(false);

  return (
    <section>
      <SectionHeader />

      {error && (
        <p
          className="font-display italic text-[0.9rem] mb-4"
          style={{ color: 'var(--color-amber)' }}
        >
          could not load registries — {String(error)}
        </p>
      )}

      {isLoading && <ListLoadingSkeleton />}

      {!isLoading && data && data.length > 0 && (
        <ul className="space-y-0">
          {data.map((row) => (
            <RegistryRow key={row.id} row={row} />
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
            <AddRegistryForm
              onCancel={() => setAddOpen(false)}
              onAdded={() => setAddOpen(false)}
            />
          </motion.div>
        )}
      </AnimatePresence>

      {!addOpen && (
        <div className="mt-8 flex justify-center">
          <PressButton onClick={() => setAddOpen(true)} tone="primary">
            + register a new registry
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
        Registered Registries
      </h2>
      <p
        className="font-display italic text-[0.8rem]"
        style={{ color: 'var(--color-ink-quiet)' }}
      >
        passwords sealed at rest, never returned to the browser
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
        a fresh page.
      </p>
      <p
        className="font-display italic text-[0.9rem] mt-2"
        style={{ color: 'var(--color-ink-quiet)' }}
      >
        register a Docker registry to enable image pulls from outside
        <br />
        your local network.
      </p>
    </div>
  );
}

// ---------- Row -----------------------------------------------------

function RegistryRow({ row }: { row: RegistryCredentialSummary }) {
  const [confirming, setConfirming] = useState(false);
  const del = useDeleteRegistry();

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
      <div className="flex items-baseline gap-3">
        <span
          aria-hidden
          className="glyph"
          style={{ color: 'var(--color-ink-faded)' }}
        >
          {row.auth_kind === 'static' ? '●' : '◇'}
        </span>
        <span
          className="font-mono"
          style={{ fontSize: '0.95rem', color: 'var(--color-ink)' }}
        >
          {row.registry_host}
        </span>
        <AuthKindChip kind={row.auth_kind} />
        {row.auth_principal && (
          <span
            className="font-mono"
            style={{ fontSize: '0.78rem', color: 'var(--color-ink-faded)' }}
          >
            {row.auth_principal}
          </span>
        )}
        <span className="ml-auto flex items-baseline gap-4">
          <span
            className="font-mono text-[0.72rem]"
            style={{ color: 'var(--color-ink-quiet)' }}
            title={new Date(row.created_at).toLocaleString()}
          >
            {timeAgo(row.created_at)}
          </span>
          {!confirming ? (
            <PressButton onClick={() => setConfirming(true)}>
              remove
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
                onClick={() => del.mutate(row.registry_host)}
                tone="danger"
                disabled={del.isPending}
              >
                {del.isPending ? 'removing…' : 'yes'}
              </PressButton>
              <PressButton onClick={() => setConfirming(false)}>
                no
              </PressButton>
            </span>
          )}
        </span>
      </div>
      {del.error && (
        <p
          className="font-display italic text-[0.85rem] mt-2"
          style={{ color: 'var(--color-amber)', marginLeft: '1.4rem' }}
        >
          could not remove — {String(del.error)}
        </p>
      )}
    </motion.li>
  );
}

function AuthKindChip({ kind }: { kind: RegistryAuthKind }) {
  const label = kind === 'static' ? 'static' : 'workload identity';
  return (
    <span
      className="font-mono smallcaps"
      style={{
        fontSize: '0.65rem',
        color: 'var(--color-ink-faded)',
        border: '1px solid var(--color-rule)',
        padding: '0.1rem 0.5rem',
        letterSpacing: '0.12em',
      }}
    >
      {label}
    </span>
  );
}

// ---------- Add form ------------------------------------------------

function AddRegistryForm({
  onCancel,
  onAdded,
}: {
  onCancel: () => void;
  onAdded: () => void;
}) {
  const [host, setHost] = useState('');
  const [authKind, setAuthKind] = useState<RegistryAuthKind>('static');
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [impersonateSa, setImpersonateSa] = useState('');
  const add = useAddRegistry();
  const [submitError, setSubmitError] = useState<string | null>(null);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setSubmitError(null);
    if (!host.trim()) {
      setSubmitError('host is required');
      return;
    }
    let auth: AddRegistryAuth;
    if (authKind === 'static') {
      if (!username.trim()) {
        setSubmitError('username is required');
        return;
      }
      if (!password) {
        setSubmitError('password is required');
        return;
      }
      auth = { kind: 'static', username: username.trim(), password };
    } else {
      auth = {
        kind: 'gcp_workload_identity',
        impersonate_sa: impersonateSa.trim() || undefined,
      };
    }
    try {
      await add.mutateAsync({ host: host.trim(), auth });
      onAdded();
    } catch (e) {
      setSubmitError(String(e));
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
      <SubHead>NEW REGISTRY</SubHead>

      <Field label="host" hint="e.g. ghcr.io · gcr.io · us-east1-docker.pkg.dev · localhost:5000">
        <input
          type="text"
          value={host}
          autoFocus
          onChange={(e) => setHost(e.target.value)}
          className="ledger-input font-mono"
          placeholder="ghcr.io"
          spellCheck={false}
          autoCapitalize="off"
        />
      </Field>

      <div className="space-y-3">
        <SubHead>AUTH MODEL</SubHead>
        <AuthKindCards value={authKind} onChange={setAuthKind} />
      </div>

      <AnimatePresence mode="wait" initial={false}>
        {authKind === 'static' ? (
          <motion.div
            key="static"
            initial={{ opacity: 0, y: 4 }}
            animate={{ opacity: 1, y: 0 }}
            exit={{ opacity: 0 }}
            transition={{ duration: 0.18 }}
            className="space-y-5"
          >
            <Field
              label="username"
              hint="for GCP service-account JSON keys, the literal string _json_key"
            >
              <input
                type="text"
                value={username}
                onChange={(e) => setUsername(e.target.value)}
                className="ledger-input font-mono"
                spellCheck={false}
                autoCapitalize="off"
                autoComplete="username"
                placeholder="username or _json_key"
              />
            </Field>
            <Field
              label="password"
              hint="sealed under the deployment KEK before it touches Postgres; never returned by the API"
            >
              <input
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                className="ledger-input font-mono"
                autoComplete="new-password"
                placeholder="•••••"
              />
            </Field>
          </motion.div>
        ) : (
          <motion.div
            key="gcp-wi"
            initial={{ opacity: 0, y: 4 }}
            animate={{ opacity: 1, y: 0 }}
            exit={{ opacity: 0 }}
            transition={{ duration: 0.18 }}
            className="space-y-5"
          >
            <p
              className="font-display italic text-[0.92rem]"
              style={{ color: 'var(--color-ink-faded)' }}
            >
              No password required. The host-agent's ambient GCP identity
              (GKE pod SA, GCE/Cloud Run instance SA) is exchanged for a
              short-lived OAuth token on every pull.
            </p>
            <Field
              label="impersonate"
              hint="optional · pull as a different service account via IAM Credentials API. Leave empty to use the ambient identity directly."
            >
              <input
                type="text"
                value={impersonateSa}
                onChange={(e) => setImpersonateSa(e.target.value)}
                className="ledger-input font-mono"
                placeholder="engram@my-project.iam.gserviceaccount.com"
                spellCheck={false}
                autoCapitalize="off"
              />
            </Field>
          </motion.div>
        )}
      </AnimatePresence>

      {submitError && <FormError message={submitError} />}

      <div className="flex items-baseline gap-6 pt-2">
        <PressButton type="submit" tone="primary" disabled={add.isPending}>
          {add.isPending ? 'sealing & saving…' : 'register'}
        </PressButton>
        <PressButton onClick={onCancel}>cancel</PressButton>
      </div>
    </form>
  );
}

interface AuthKindCardSpec {
  kind: RegistryAuthKind | 'aws_instance_role'; // includes the stub
  label: string;
  blurb: string;
  disabled?: boolean;
  hint?: string;
}

const KIND_CARDS: AuthKindCardSpec[] = [
  {
    kind: 'static',
    label: 'Static',
    blurb:
      'Username + password sealed under the deployment KEK. DockerHub, GHCR, Quay, Harbor, GAR with a service-account JSON key.',
  },
  {
    kind: 'gcp_workload_identity',
    label: 'GCP Workload Identity',
    blurb:
      'Ambient GCP identity exchanged for a short-lived token per pull. No stored secret material.',
  },
  {
    kind: 'aws_instance_role',
    label: 'AWS Instance Role',
    blurb:
      'Ambient AWS IAM identity exchanged for an ECR token per pull. Same shape as GCP WI.',
    disabled: true,
    hint: 'coming soon',
  },
];

function AuthKindCards({
  value,
  onChange,
}: {
  value: RegistryAuthKind;
  onChange: (v: RegistryAuthKind) => void;
}) {
  return (
    <div
      role="radiogroup"
      aria-label="Authentication model"
      className="grid gap-4"
      style={{ gridTemplateColumns: 'repeat(auto-fit, minmax(13rem, 1fr))' }}
    >
      {KIND_CARDS.map((card) => {
        const selected = !card.disabled && card.kind === value;
        return (
          <button
            key={card.kind}
            type="button"
            role="radio"
            aria-checked={selected}
            disabled={card.disabled}
            onClick={() => {
              if (!card.disabled) onChange(card.kind as RegistryAuthKind);
            }}
            className="text-left p-4 transition-colors"
            style={{
              backgroundColor: selected
                ? 'var(--color-paper-warm)'
                : 'transparent',
              border: `1px solid ${
                selected ? 'var(--color-ink)' : 'var(--color-rule)'
              }`,
              cursor: card.disabled ? 'not-allowed' : 'pointer',
              opacity: card.disabled ? 0.55 : 1,
            }}
          >
            <div className="flex items-baseline gap-2">
              <span
                aria-hidden
                style={{
                  color: selected
                    ? 'var(--color-ink)'
                    : 'var(--color-ink-quiet)',
                  fontSize: '0.85rem',
                  width: '0.9rem',
                  display: 'inline-block',
                }}
              >
                {selected ? '●' : '○'}
              </span>
              <span
                className="font-display"
                style={{ fontSize: '0.95rem', color: 'var(--color-ink)' }}
              >
                {card.label}
              </span>
              {card.hint && (
                <span
                  className="font-mono smallcaps text-[0.62rem] ml-auto"
                  style={{
                    color: 'var(--color-ink-quiet)',
                    letterSpacing: '0.16em',
                  }}
                >
                  {card.hint}
                </span>
              )}
            </div>
            <p
              className="font-display italic text-[0.82rem] mt-2"
              style={{
                color: 'var(--color-ink-quiet)',
                lineHeight: 1.45,
                marginLeft: '1.3rem',
              }}
            >
              {card.blurb}
            </p>
          </button>
        );
      })}
    </div>
  );
}

// ---------- helpers -------------------------------------------------

/** Render an ISO timestamp as "3m ago" / "2h ago" / "5d ago" / a
 * date for older entries. Notebook-flavoured: short, lowercase,
 * mono-friendly. */
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
