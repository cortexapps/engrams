import { AnimatePresence, motion } from 'framer-motion';
import { useState } from 'react';
import {
  useAddHarnessPack,
  useDeleteHarnessPack,
  useHarnessPacks,
} from '../../hooks/useHarnessPacks';
import type { HarnessPackSummary } from '../../types';
import { Field, FormError, PressButton, SubHead } from './_form';

// Harness packs panel. The list shape mirrors RegistriesPanel — a
// row per registered name, with an inline-expand "+ register a new"
// prompt. The form is simpler than the registry one (no auth-kind
// branching) because harness packs are pointer-only: the bytes live
// in a Docker registry already, this surface just maintains the
// `(name → URI)` index sessions select by.

export function HarnessesPanel() {
  const { data, isLoading, error } = useHarnessPacks();
  const [addOpen, setAddOpen] = useState(false);

  return (
    <section>
      <SectionHeader />

      {error && (
        <p
          className="font-display italic text-[0.9rem] mb-4"
          style={{ color: 'var(--color-amber)' }}
        >
          could not load harness packs — {String(error)}
        </p>
      )}

      {isLoading && (
        <p
          className="font-display italic text-[0.9rem] py-3"
          style={{ color: 'var(--color-ink-quiet)' }}
        >
          loading…
        </p>
      )}

      {!isLoading && data && data.length > 0 && (
        <ul className="space-y-0">
          {data.map((row) => (
            <HarnessRow key={row.name} row={row} />
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
            <AddHarnessForm
              onCancel={() => setAddOpen(false)}
              onAdded={() => setAddOpen(false)}
            />
          </motion.div>
        )}
      </AnimatePresence>

      {!addOpen && (
        <div className="mt-8 flex justify-center">
          <PressButton onClick={() => setAddOpen(true)} tone="primary">
            + register a new harness pack
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
        Registered Harness Packs
      </h2>
      <p
        className="font-display italic text-[0.8rem]"
        style={{ color: 'var(--color-ink-quiet)' }}
      >
        the host-agent pulls these on first session, caches by digest
      </p>
    </header>
  );
}

function EmptyState() {
  return (
    <div className="py-10 text-center" style={{ minHeight: '8rem' }}>
      <p
        className="font-display italic text-[1.05rem]"
        style={{ color: 'var(--color-ink-faded)' }}
      >
        no harnesses yet.
      </p>
      <p
        className="font-display italic text-[0.9rem] mt-2"
        style={{ color: 'var(--color-ink-quiet)' }}
      >
        push a pack to a registry, then register its name + URI here.
        <br />
        sessions will pick it up by name in the next create.
      </p>
    </div>
  );
}

function HarnessRow({ row }: { row: HarnessPackSummary }) {
  const [confirming, setConfirming] = useState(false);
  const del = useDeleteHarnessPack();
  // Legacy host-resident packs can't be deleted from the UI — they
  // live on the host's filesystem under cfg.harnesses_dir, not in
  // Postgres. Showing the row is useful (the operator knows the
  // pack is reachable to sessions); offering a delete button would
  // be a lie.
  const isLegacy = row.registry_uri == null;

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
          {isLegacy ? '◌' : '●'}
        </span>
        <span
          className="font-mono"
          style={{ fontSize: '0.95rem', color: 'var(--color-ink)' }}
        >
          {row.name}
        </span>
        {isLegacy ? (
          <span
            className="font-mono smallcaps"
            style={{
              fontSize: '0.65rem',
              color: 'var(--color-ink-quiet)',
              border: '1px solid var(--color-rule)',
              padding: '0.1rem 0.5rem',
              letterSpacing: '0.12em',
            }}
            title="Lives on the host filesystem under cfg.harnesses_dir"
          >
            host-resident
          </span>
        ) : (
          <span
            className="font-mono"
            style={{
              fontSize: '0.78rem',
              color: 'var(--color-ink-faded)',
            }}
          >
            {row.registry_uri}
          </span>
        )}
        <span className="ml-auto flex items-baseline gap-4">
          {!isLegacy && !confirming && (
            <PressButton onClick={() => setConfirming(true)}>remove</PressButton>
          )}
          {!isLegacy && confirming && (
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
                onClick={() => del.mutate(row.name)}
                tone="danger"
                disabled={del.isPending}
              >
                {del.isPending ? 'removing…' : 'yes'}
              </PressButton>
              <PressButton onClick={() => setConfirming(false)}>no</PressButton>
            </span>
          )}
        </span>
      </div>
      {row.description && (
        <p
          className="font-display italic text-[0.85rem] mt-1"
          style={{
            color: 'var(--color-ink-quiet)',
            marginLeft: '1.4rem',
          }}
        >
          {row.description}
        </p>
      )}
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

function AddHarnessForm({
  onCancel,
  onAdded,
}: {
  onCancel: () => void;
  onAdded: () => void;
}) {
  const [name, setName] = useState('');
  const [registryUri, setRegistryUri] = useState('');
  const [description, setDescription] = useState('');
  const add = useAddHarnessPack();
  const [submitError, setSubmitError] = useState<string | null>(null);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setSubmitError(null);
    if (!name.trim()) {
      setSubmitError('name is required');
      return;
    }
    if (!registryUri.trim()) {
      setSubmitError('registry URI is required');
      return;
    }
    try {
      await add.mutateAsync({
        name: name.trim(),
        registry_uri: registryUri.trim(),
        description: description.trim() || undefined,
      });
      onAdded();
    } catch (e) {
      setSubmitError(String(e));
    }
  };

  return (
    <form
      onSubmit={submit}
      className="space-y-6 pb-4"
      style={{
        borderTop: '1px solid var(--color-rule)',
        borderBottom: '1px solid var(--color-rule)',
        paddingTop: '1.5rem',
      }}
    >
      <SubHead>NEW HARNESS PACK</SubHead>

      <Field
        label="name"
        hint="the value sessions select by — must be unique within this deployment"
      >
        <input
          type="text"
          value={name}
          autoFocus
          onChange={(e) => setName(e.target.value)}
          className="ledger-input font-mono"
          placeholder="claude"
          spellCheck={false}
          autoCapitalize="off"
        />
      </Field>

      <Field
        label="registry URI"
        hint="full OCI URI, e.g. ghcr.io/cortex/harness-claude:v1.2 — bytes must already exist in the registry"
      >
        <input
          type="text"
          value={registryUri}
          onChange={(e) => setRegistryUri(e.target.value)}
          className="ledger-input font-mono"
          placeholder="ghcr.io/cortex/harness-claude:v1.2"
          spellCheck={false}
          autoCapitalize="off"
        />
      </Field>

      <Field label="description" hint="optional · short label for the dashboard">
        <input
          type="text"
          value={description}
          onChange={(e) => setDescription(e.target.value)}
          className="ledger-input font-display"
          placeholder="Anthropic Claude Code adapter"
        />
      </Field>

      {submitError && <FormError message={submitError} />}

      <div className="flex items-baseline gap-6 pt-2">
        <PressButton type="submit" tone="primary" disabled={add.isPending}>
          {add.isPending ? 'registering…' : 'register'}
        </PressButton>
        <PressButton onClick={onCancel}>cancel</PressButton>
      </div>
    </form>
  );
}
