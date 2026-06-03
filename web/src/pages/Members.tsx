// ADR 0031: Members admin surface. Lists all users in the deployment with
// inline role and activation controls. Gated by RequireAdmin.
//
// Grid: person | role | source | status · actions
// Columns: 2.4fr 0.8fr 1.3fr 1.7fr

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { fetchUsers, updateUser } from '../api';
import { useAuth } from '../auth/AuthProvider';
import { PersonMark, RoleTag, Provenance, MemberStatus } from '../components/Identity';
import type { AdminUser, Role } from '../types';

export function Members() {
  const { principal } = useAuth();
  const queryClient = useQueryClient();

  const { data: users = [], isLoading, error } = useQuery({
    queryKey: ['admin', 'users'],
    queryFn: fetchUsers,
  });

  const mutation = useMutation({
    mutationFn: ({
      id,
      patch,
    }: {
      id: string;
      patch: { role?: Role; active?: boolean };
    }) => updateUser(id, patch),
    onSuccess: (updated) => {
      queryClient.setQueryData<AdminUser[]>(['admin', 'users'], (prev) =>
        prev ? prev.map((u) => (u.id === updated.id ? updated : u)) : [updated],
      );
    },
  });

  const setRole = (id: string, role: Role) =>
    mutation.mutate({ id, patch: { role } });

  const setActive = (id: string, active: boolean) =>
    mutation.mutate({ id, patch: { active } });

  const admins = users.filter((u) => u.role === 'admin' && u.active).length;
  const disabled = users.filter((u) => !u.active).length;

  if (isLoading) {
    return (
      <section>
        <p className="font-display italic" style={{ color: 'var(--color-ink-quiet)' }}>
          loading…
        </p>
      </section>
    );
  }

  if (error) {
    return (
      <section>
        <p className="font-display italic" style={{ color: 'var(--color-ink-quiet)' }}>
          could not load members — {(error as Error).message}
        </p>
      </section>
    );
  }

  return (
    <section>
      <header className="mb-6 flex items-baseline justify-between">
        <h2 className="section-label">Members</h2>
        <span className="font-display italic text-[0.8rem]" style={{ color: 'var(--color-ink-quiet)' }}>
          everyone in this deployment
        </span>
      </header>

      <div className="people-rollup">
        <b>{users.length}</b> people{' '}
        <span className="dot">·</span>{' '}
        <b>{admins}</b> admins{' '}
        <span className="dot">·</span>{' '}
        <b>{disabled}</b> disabled
      </div>

      <div className="members-ledger">
        {/* Header row */}
        <div className="members-row members-head">
          <span className="section-label">person</span>
          <span className="section-label">role</span>
          <span className="section-label">source</span>
          <span className="section-label" style={{ textAlign: 'right' }}>
            status · actions
          </span>
        </div>

        {users.map((user) => {
          const isYou = user.email === principal.email;
          return (
            <MemberRow
              key={user.id}
              user={user}
              isYou={isYou}
              onSetRole={(role) => setRole(user.id, role)}
              onSetActive={(active) => setActive(user.id, active)}
            />
          );
        })}
      </div>

      <p className="ledger-note">
        roles are provisioned from your identity provider on first sign-in and
        stay in sync over SCIM; promote or revoke here and the change is marked{' '}
        <em>set by an admin</em>. a deactivated member keeps their sessions but
        can't sign in.
      </p>
    </section>
  );
}

function MemberRow({
  user,
  isYou,
  onSetRole,
  onSetActive,
}: {
  user: AdminUser;
  isYou: boolean;
  onSetRole: (role: Role) => void;
  onSetActive: (active: boolean) => void;
}) {
  return (
    <div className={`members-row${user.active ? '' : ' is-off'}`}>
      {/* Person */}
      <div className={`person${user.active ? '' : ' is-off'}`}>
        <PersonMark
          name={user.display_name}
          email={user.email}
          size="sm"
          off={!user.active}
        />
        <div className="person-id">
          <div className="person-name">
            {user.display_name || user.email}
            {isYou && <span className="you">you</span>}
          </div>
          <div className="person-email">{user.email}</div>
        </div>
      </div>

      {/* Role */}
      <span>
        <RoleTag role={user.role} />
      </span>

      {/* Provenance */}
      <Provenance source={user.role_source} />

      {/* Status + actions */}
      <div className="members-actions">
        <MemberStatus active={user.active} />
        {isYou ? (
          <span className="members-self">you</span>
        ) : (
          <>
            {user.role === 'member' ? (
              <button
                type="button"
                className="members-act act-primary"
                onClick={() => onSetRole('admin')}
              >
                make admin
              </button>
            ) : (
              <button
                type="button"
                className="members-act act-quiet"
                onClick={() => onSetRole('member')}
              >
                revoke admin
              </button>
            )}
            {user.active ? (
              <button
                type="button"
                className="members-act act-quiet"
                onClick={() => onSetActive(false)}
              >
                deactivate
              </button>
            ) : (
              <button
                type="button"
                className="members-act act-primary"
                onClick={() => onSetActive(true)}
              >
                reactivate
              </button>
            )}
          </>
        )}
      </div>
    </div>
  );
}
