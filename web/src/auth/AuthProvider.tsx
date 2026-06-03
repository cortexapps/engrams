// ADR 0031 auth context. Owns the `GET /me` query and gates the app behind a
// resolved principal. A 401 is handled in the api client (hard redirect to
// /auth/login), so it never reaches here; this provider renders:
//   - boot screen while `/me` is in flight
//   - "not a member" screen for a 403 (authenticated but not provisioned)
//   - error screen for non-401/403 failures (coordinator down, etc.)

import { useQuery } from '@tanstack/react-query';
import { createContext, useContext, type ReactNode } from 'react';
import { fetchMe, logout, NotMemberError } from '../api';
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

  if (error instanceof NotMemberError) {
    return <NotMemberScreen email={error.email} />;
  }

  if (error || !principal) {
    return (
      <AuthErrorScreen
        message={error?.message}
        onRetry={() => void refetch()}
      />
    );
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

// ---- Auth state screens --------------------------------------------------
// Full-viewport, centered on --bg, 32rem card, engram mark, lowercase em-dash voice.

function BootScreen() {
  return (
    <div className="auth-stage">
      <div className="auth-card">
        <span className="auth-mark">
          <EngramMark size={72} mode="loop" />
        </span>
        <div className="auth-line">authenticating…</div>
      </div>
    </div>
  );
}

function AuthErrorScreen({
  message,
  onRetry,
}: {
  message?: string;
  onRetry: () => void;
}) {
  return (
    <div className="auth-stage">
      <div className="auth-card">
        <span className="auth-mark">
          <EngramMark size={72} mode="static" />
        </span>
        <div className="auth-strong">
          could not reach the coordinator —<br />retrying…
        </div>
        {message && <div className="auth-detail">{message}</div>}
        <div className="auth-actions">
          <button
            type="button"
            className="members-act"
            onClick={onRetry}
          >
            retry now
          </button>
        </div>
      </div>
    </div>
  );
}

function NotMemberScreen({ email }: { email: string }) {
  return (
    <div className="auth-stage">
      <div className="auth-card">
        <span className="auth-mark">
          <EngramMark size={72} mode="static" />
        </span>
        <div className="auth-strong">
          you're signed in — but not yet<br />a member of this deployment.
        </div>
        {email && <div className="auth-detail">{email}</div>}
        <div className="auth-line">
          ask an admin to add you, then reload.
        </div>
        <div className="auth-actions">
          <button
            type="button"
            className="members-act act-quiet"
            onClick={() => void logout()}
          >
            sign out
          </button>
        </div>
      </div>
    </div>
  );
}
