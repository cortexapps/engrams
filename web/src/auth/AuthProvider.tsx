// ADR 0031 auth context. Owns the `GET /me` query and gates the app behind a
// resolved principal. A 401 is handled in the api client (hard redirect to
// /auth/login), so it never reaches here; this provider only renders a boot
// state while `/me` is in flight and an inline error for non-401 failures
// (e.g. the coordinator is down — redirecting would loop).
//
// In dev (synthetic admin) `/me` always resolves to a local admin, so there is
// no login wall locally — the boot screen flashes and the app renders.

import { useQuery } from '@tanstack/react-query';
import { createContext, useContext, type ReactNode } from 'react';
import { fetchMe } from '../api';
import type { Principal } from '../types';
import { EngramMark } from '../components/EngramMark';

export interface AuthState {
  /** Always present for children — the provider only renders them once the
   * principal has resolved. */
  principal: Principal;
  isAdmin: boolean;
  /** Re-fetch `/me` (e.g. after saving a Claude token flips has_claude_token). */
  refresh: () => void;
}

const AuthContext = createContext<AuthState | null>(null);

/** Test seam: wrap children with a fixed principal, bypassing the `/me`
 * query. Used by `renderWithProviders` so component tests don't each need a
 * `/me` fetch mock. */
export function AuthContextProvider({
  value,
  children,
}: {
  value: AuthState;
  children: ReactNode;
}) {
  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}

export function AuthProvider({ children }: { children: ReactNode }) {
  const { data: principal, isLoading, error, refetch } = useQuery({
    queryKey: ['me'],
    queryFn: fetchMe,
    retry: 0,
    staleTime: 60_000,
    refetchOnWindowFocus: false,
  });

  if (isLoading) {
    return <BootScreen />;
  }

  if (error || !principal) {
    return <AuthErrorScreen message={error?.message} onRetry={() => void refetch()} />;
  }

  const value: AuthState = {
    principal,
    isAdmin: principal.is_admin,
    refresh: () => void refetch(),
  };
  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}

export function useAuth(): AuthState {
  const ctx = useContext(AuthContext);
  if (!ctx) {
    throw new Error('useAuth must be used within an AuthProvider');
  }
  return ctx;
}

export function useIsAdmin(): boolean {
  return useAuth().isAdmin;
}

function BootScreen() {
  return (
    <div
      style={{
        minHeight: '100vh',
        display: 'grid',
        placeItems: 'center',
        background: 'var(--color-paper)',
      }}
    >
      <div style={{ textAlign: 'center' }}>
        <EngramMark mode="loop" size={72} />
        <div
          style={{
            marginTop: '1rem',
            fontStyle: 'italic',
            color: 'var(--color-ink-quiet)',
          }}
        >
          authenticating…
        </div>
      </div>
    </div>
  );
}

function AuthErrorScreen({ message, onRetry }: { message?: string; onRetry: () => void }) {
  return (
    <div
      style={{
        minHeight: '100vh',
        display: 'grid',
        placeItems: 'center',
        background: 'var(--color-paper)',
      }}
    >
      <div style={{ textAlign: 'center', maxWidth: '28rem' }}>
        <EngramMark mode="static" size={72} />
        <div style={{ marginTop: '1rem', color: 'var(--color-ink)' }}>
          Couldn’t reach the coordinator.
        </div>
        {message ? (
          <div
            style={{
              marginTop: '0.5rem',
              fontSize: '0.85rem',
              color: 'var(--color-ink-quiet)',
            }}
          >
            {message}
          </div>
        ) : null}
        <button
          type="button"
          onClick={onRetry}
          style={{ marginTop: '1rem', cursor: 'pointer' }}
        >
          retry
        </button>
      </div>
    </div>
  );
}
