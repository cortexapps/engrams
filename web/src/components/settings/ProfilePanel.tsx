// Placeholder for the eventual user-profile surface. Today the
// coordinator has no notion of a logged-in user — the bearer-token
// auth in `cfg.auth_tokens` is a deployment-wide allowlist, not a
// per-user identity. When a real auth model lands (GitHub App,
// OIDC, etc.), this panel grows to show the user's email, group
// memberships, and any per-user settings (default harness, default
// image, etc.).
//
// For now we just acknowledge the gap so the route doesn't 404 and
// the navigation feels complete.

export function ProfilePanel() {
  return (
    <section>
      <header className="mb-6 flex items-baseline justify-between">
        <h2
          className="font-mono smallcaps text-[0.7rem]"
          style={{ color: 'var(--color-ink-quiet)', letterSpacing: '0.18em' }}
        >
          Profile
        </h2>
        <p
          className="font-display italic text-[0.8rem]"
          style={{ color: 'var(--color-ink-quiet)' }}
        >
          deployment-wide auth, not per-user — yet
        </p>
      </header>

      <div className="py-12 text-center" style={{ minHeight: '14rem' }}>
        <p
          className="font-display italic"
          style={{ fontSize: '1.4rem', color: 'var(--color-ink-faded)' }}
        >
          no identity yet.
        </p>
        <p
          className="font-display italic text-[0.95rem] mt-3"
          style={{ color: 'var(--color-ink-quiet)', maxWidth: '36rem' }}
        >
          Engram authenticates clients with a deployment-wide bearer token
          configured at coordinator startup — there's no user identity to
          render here. Per-user auth (GitHub App / OIDC) lands with the
          first hosted deployment.
        </p>
        <p
          className="font-mono smallcaps text-[0.7rem] mt-8"
          style={{
            color: 'var(--color-ink-quiet)',
            letterSpacing: '0.18em',
          }}
        >
          ENGRAM_AUTH_TOKENS · deployment KEK · Postgres
        </p>
      </div>
    </section>
  );
}
