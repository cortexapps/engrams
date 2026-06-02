// ADR 0031 user setting: the signed-in identity. Reads the resolved principal
// (email, display name, role) from the auth context. Role/membership are
// driven by the IdP (JIT now, SCIM later) + admin promotion, so they're shown
// read-only here; the Claude token lives on its own sub-tab.

import { Link } from 'react-router-dom';
import { useAuth } from '../../auth/AuthProvider';

export function ProfilePanel() {
  const { principal } = useAuth();
  const initial = (principal.display_name || principal.email).charAt(0).toUpperCase();

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
          signed in
        </p>
      </header>

      <div className="flex items-center gap-4 mb-8">
        <div
          className="grid place-items-center font-mono"
          style={{
            width: '3rem',
            height: '3rem',
            borderRadius: '0.4rem',
            background: 'var(--color-paper-warm)',
            border: '1px solid var(--color-rule)',
            color: 'var(--color-ink)',
            fontSize: '1.2rem',
          }}
          aria-hidden
        >
          {initial}
        </div>
        <div>
          <div className="font-display" style={{ fontSize: '1.2rem', color: 'var(--color-ink)' }}>
            {principal.display_name || principal.email}
          </div>
          <div
            className="font-mono text-[0.82rem]"
            style={{ color: 'var(--color-ink-quiet)' }}
          >
            {principal.email}
          </div>
        </div>
      </div>

      <dl className="space-y-3" style={{ maxWidth: '34rem' }}>
        <Row label="role">
          <span style={{ color: 'var(--color-ink)' }}>{principal.role}</span>
          {principal.is_admin && (
            <span
              className="font-display italic text-[0.8rem] ml-2"
              style={{ color: 'var(--color-ink-quiet)' }}
            >
              · sees fleet, storage &amp; global settings
            </span>
          )}
        </Row>
        <Row label="Claude token">
          {principal.has_claude_token ? (
            <span style={{ color: 'var(--color-ink)' }}>saved</span>
          ) : (
            <Link
              to="/settings/tokens"
              className="font-display italic"
              style={{ color: 'var(--color-amber)' }}
            >
              not saved — add one →
            </Link>
          )}
        </Row>
      </dl>
    </section>
  );
}

function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="grid items-baseline gap-x-4" style={{ gridTemplateColumns: '8rem 1fr' }}>
      <dt
        className="font-mono smallcaps text-[0.7rem]"
        style={{ color: 'var(--color-ink-quiet)', letterSpacing: '0.12em' }}
      >
        {label}
      </dt>
      <dd className="font-display text-[0.95rem]">{children}</dd>
    </div>
  );
}
