// Storage — COW state's real home (`/storage`). Fleet-wide chunk /
// durability rollups + a per-sandbox durability ledger. Wired to the
// coordinator's storage-summary endpoint in a follow-up commit; this
// is the surface shell.

export function Storage() {
  return (
    <main className="book-wide surface">
      <div className="surface-head">
        <div>
          <h1 className="surface-title">storage</h1>
          <p className="surface-sub">
            content-addressed chunk store, snapshots, and copy-on-write
            durability.
          </p>
        </div>
      </div>
      <p className="font-display italic" style={{ color: 'var(--color-ink-quiet)' }}>
        durability ledger — wiring to the coordinator.
      </p>
    </main>
  );
}
