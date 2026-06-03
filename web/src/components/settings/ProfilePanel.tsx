// ADR 0031 redesign: signed-in identity, role & provenance, access legend.
// Square person mark (fixes the shipped border-radius:0.4rem avatar),
// RoleTag + Provenance in the definition list, and an access legend that
// makes the role model legible at a glance.

import { Link } from 'react-router-dom';
import { useAuth } from '../../auth/AuthProvider';
import { PersonMark, RoleTag, Provenance } from '../Identity';

export function ProfilePanel() {
  const { principal } = useAuth();

  return (
    <section>
      <header className="mb-6 flex items-baseline justify-between">
        <h2 className="section-label">Profile</h2>
        <p className="font-display italic text-[0.8rem]" style={{ color: 'var(--color-ink-quiet)' }}>
          signed in
        </p>
      </header>

      {/* Square person mark + name + email — the bug fix: no border-radius */}
      <div className="profile-head">
        <PersonMark name={principal.display_name} email={principal.email} size="md" />
        <div>
          <div className="font-display" style={{ fontSize: '1.2rem', color: 'var(--color-ink)' }}>
            {principal.display_name || principal.email}
          </div>
          <div className="font-mono text-[0.82rem]" style={{ color: 'var(--color-ink-quiet)' }}>
            {principal.email}
          </div>
        </div>
      </div>

      <dl style={{ maxWidth: '40rem' }}>
        <div className="profile-row">
          <dt>role</dt>
          <dd className="flex items-baseline gap-3" style={{ flexWrap: 'wrap' }}>
            <RoleTag role={principal.role} />
            {principal.role_source && <Provenance source={principal.role_source} />}
          </dd>
        </div>
        <div className="profile-row">
          <dt>tokens</dt>
          <dd>
            {principal.has_claude_token ? (
              <span>
                Claude Code saved{' '}
                <span style={{ color: 'var(--color-ink-quiet)' }}>
                  · managed under{' '}
                  <Link to="/settings/tokens" style={{ color: 'var(--color-ink-quiet)' }}>
                    Tokens
                  </Link>
                </span>
              </span>
            ) : (
              <Link
                to="/settings/tokens"
                className="font-display italic"
                style={{ color: 'var(--color-amber)' }}
              >
                none saved — add one under Tokens →
              </Link>
            )}
          </dd>
        </div>
      </dl>

      <AccessLegend role={principal.role} />
    </section>
  );
}

function AccessLegend({ role }: { role: string }) {
  const isAdmin = role === 'admin';
  const can = isAdmin
    ? [
        'launch & manage your own sessions',
        'oversee every session across the fleet',
        'inspect host capacity & drain hosts',
        'read storage durability & snapshots',
        'curate images & registry credentials',
        'manage members & their roles',
      ]
    : [
        'launch & manage your own sessions',
        'save your own Claude Code token',
      ];
  const cannot = isAdmin
    ? []
    : ['the fleet, storage & deployment settings — admin only'];

  return (
    <div className="access-legend">
      <span className="section-label">what your role can do</span>
      <ul className="access-list">
        {can.map((c) => (
          <li key={c}>
            <span className="glyph" style={{ color: 'var(--accent-archived)' }}>✓</span>
            {c}
          </li>
        ))}
        {cannot.map((c) => (
          <li key={c} className="denied">
            <span className="glyph">✕</span>
            {c}
          </li>
        ))}
      </ul>
    </div>
  );
}
