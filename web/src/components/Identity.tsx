// ADR 0031 identity vocabulary — the "people" layer of the Lab Notebook system.
// Typography + hairline rules only; no new hues, no rounded corners.

import { EngramMark } from './EngramMark';

// --- PersonMark -----------------------------------------------------------
// Embossed typesetter's initial on --bg-raised in a square hairline frame.
// Square at every size — fixes the shipped border-radius:0.4rem profile avatar.

export function PersonMark({
  name,
  email,
  size = 'sm',
  off = false,
}: {
  name?: string | null;
  email?: string | null;
  size?: 'xs' | 'sm' | 'md';
  off?: boolean;
}) {
  const initial = ((name || email || '?').charAt(0) || '?').toUpperCase();
  return (
    <span
      className={`person-mark pm-${size}${off ? ' pm-off' : ''}`}
      aria-hidden="true"
    >
      <span>{initial}</span>
    </span>
  );
}

// --- OwnerBadge -----------------------------------------------------------
// A person mark, or the engram trace mark for platform-owned sessions.

export function OwnerBadge({
  kind,
  name,
  email,
  size = 'xs',
  off = false,
}: {
  kind?: 'user' | 'system' | null;
  name?: string | null;
  email?: string | null;
  size?: 'xs' | 'sm' | 'md';
  off?: boolean;
}) {
  if (kind === 'system') {
    const px = size === 'xs' ? 15 : 19;
    return (
      <span className={`person-mark pm-${size} pm-system`} title="engrams · automated">
        <EngramMark size={px} mode="static" />
      </span>
    );
  }
  return <PersonMark name={name} email={email} size={size} off={off} />;
}

// --- OwnerCell ------------------------------------------------------------
// The full owner token in a session row: badge + label.

export function OwnerCell({
  ownerKind,
  ownerName,
  ownerEmail,
}: {
  ownerKind?: 'user' | 'system' | null;
  ownerName?: string | null;
  ownerEmail?: string | null;
}) {
  const isSystem = ownerKind === 'system' || (!ownerEmail && !ownerName);
  return (
    <span
      className="owner-cell"
      title={
        isSystem
          ? 'engrams · automated'
          : `owner · ${ownerEmail ?? ownerName ?? ''}`
      }
    >
      <OwnerBadge kind={isSystem ? 'system' : 'user'} name={ownerName} email={ownerEmail} size="xs" />
      {isSystem ? (
        <span className="owner-sys">engrams</span>
      ) : (
        <span className="owner-email">{ownerEmail}</span>
      )}
    </span>
  );
}

// --- RoleTag --------------------------------------------------------------
// Square hairline box, mono all-small-caps. admin is heavier (ink + ink border).

export function RoleTag({ role }: { role: string }) {
  return <span className={`role-tag role-${role}`}>{role}</span>;
}

// --- Provenance -----------------------------------------------------------
// A faint Newsreader-italic note for role_source.

const PROVENANCE_LABELS: Record<string, string> = {
  claim: 'via auth claim',
  scim: 'via scim sync',
  manual: 'set by an admin',
};

export function Provenance({ source }: { source?: string | null }) {
  if (!source) return null;
  return (
    <span className="provenance">{PROVENANCE_LABELS[source] ?? source}</span>
  );
}

// --- MemberStatus ---------------------------------------------------------
// ink only; ✕ prefixes disabled.

export function MemberStatus({ active }: { active: boolean }) {
  return active ? (
    <span className="member-status">active</span>
  ) : (
    <span className="member-status">
      <span className="x">✕</span>disabled
    </span>
  );
}
