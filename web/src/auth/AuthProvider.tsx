// ADR 0039 §5 / Task 22 — AuthProvider recomposed on better-auth.
//
// The external interface (AuthState: principal, isAdmin, refresh) is
// UNCHANGED — every consumer (UserChip, ProfilePanel, TokensPanel,
// NewSessionDialog, Members, RequireAdmin, router.ts guards) keeps working.
//
// Internal recomposition:
//   - authn + role:  authClient.useSession() — reads the better-auth session
//     cookie (HttpOnly, same-origin). The admin plugin writes `role` onto the
//     user object; `role === 'admin'` drives isAdmin.
//   - has_claude_token: GET /api/v1/me/claude-token (orchestrator sealed-store
//     route). The vite proxy has an exact-path rule for this path → :8787
//     BEFORE the general /api/v1 → :8090 coordinator rule; Task 28 collapses
//     this once all /api/v1 traffic moves to the orchestrator.
//   - Unauthenticated path: AuthProvider renders children even when session is
//     null (resolved-unauthenticated). The auth gate lives in the router's
//     appLayoutRoute.beforeLoad (see router.tsx), which redirects to /login.
//     This avoids the infinite-reload loop that window.location.replace("/login")
//     caused before RouterProvider mounted.
//   - Orchestrator-down path: authClient.useSession() error → AuthErrorScreen
//     with retry (not the login redirect — unreachable backend ≠ signed-out).
//   - sign-out: authClient.signOut() + window.location.assign("/login").
//
// Principal shape compatibility:
//   The Principal type (types.ts) was written for the coordinator's GET /me
//   response. We synthesise an equivalent object from the better-auth session
//   so that every consumer reads `principal.email`, `principal.role`,
//   `principal.is_admin`, `principal.has_claude_token`, `principal.display_name`,
//   and `principal.can_sign_out` exactly as before.

import { useQuery } from "@tanstack/react-query";
import { createContext, useContext, type ReactNode } from "react";
import { authClient } from "@/lib/auth-client";
import type { Principal } from "../types";
import { EngramMark } from "../components/EngramMark";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";

export interface AuthState {
  /** Always present for children — the provider only renders them once the
   * principal has resolved. */
  principal: Principal;
  isAdmin: boolean;
  /** Re-fetch the claude-token presence flag (e.g. after saving a token
   * flips has_claude_token). The better-auth session itself is live via
   * authClient.useSession() and needs no manual refresh. */
  refresh: () => void;
}

const AuthContext = createContext<AuthState | null>(null);

/** Test seam: wrap children with a fixed principal, bypassing auth queries.
 * Used by `renderWithProviders` so component tests don't each need mocks. */
export function AuthContextProvider({
  value,
  children,
}: {
  value: AuthState;
  children: ReactNode;
}) {
  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}

// ---- Token presence query -----------------------------------------------
// GET /api/v1/me/claude-token → { has_claude_token: boolean }
// Routed to the orchestrator (:8787) via the exact-path vite proxy rule
// added in Task 22. Task 28 removes the special rule once /api/v1 fully
// moves to the orchestrator.
async function fetchClaudeTokenPresence(): Promise<boolean> {
  const res = await fetch("/api/v1/me/claude-token", {
    headers: { Accept: "application/json" },
    credentials: "include",
  });
  if (!res.ok) {
    // 401 → not authenticated (session lapsed); treat as no token present
    // rather than throwing so we don't block the auth render cycle.
    if (res.status === 401) return false;
    throw new Error(`/api/v1/me/claude-token → ${res.status}`);
  }
  const body = (await res.json()) as { has_claude_token?: boolean };
  return body.has_claude_token === true;
}

// ---- AuthProvider --------------------------------------------------------

export function AuthProvider({ children }: { children: ReactNode }) {
  // better-auth session — provides authn + role via the admin plugin.
  // `isPending` is true only on the very first render before the cookie
  // round-trip completes; thereafter it's synchronous from the in-memory cache.
  // `error` is non-null when the orchestrator is unreachable (network/500).
  const {
    data: session,
    isPending: sessionPending,
    error: sessionError,
    refetch: refetchSession,
  } = authClient.useSession();

  // Claude-token presence — only fetched when the session is resolved + present.
  const {
    data: hasClaudeToken,
    isLoading: tokenLoading,
    refetch: refetchToken,
  } = useQuery({
    queryKey: ["me", "claude-token"],
    queryFn: fetchClaudeTokenPresence,
    enabled: !!session,
    retry: 1,
    staleTime: 60_000,
    refetchOnWindowFocus: false,
  });

  // Boot screen — waiting for the session cookie round-trip.
  if (sessionPending) {
    return <BootScreen />;
  }

  // Orchestrator unreachable — backend error is not the same as signed-out.
  // Show a retry screen rather than redirecting to /login (which would be
  // misleading and unhelpful when the server is simply down).
  if (sessionError) {
    return <AuthErrorScreen message={sessionError.message} onRetry={() => void refetchSession()} />;
  }

  // Session resolved (null = unauthenticated, or a valid session object).
  // The auth gate lives in the router (appLayoutRoute.beforeLoad in router.tsx)
  // so that /login itself is never caught in a redirect loop. AuthProvider
  // always renders children at this point — the router decides what to render.
  if (!session) {
    // No principal — pass null context; router redirects unauthenticated routes.
    return <AuthContext.Provider value={null}>{children}</AuthContext.Provider>;
  }

  // Session resolved but token presence query in flight — show boot screen
  // briefly rather than flashing the app without token info.
  if (tokenLoading) {
    return <BootScreen />;
  }

  // The admin plugin writes role as 'admin' | 'user'. We treat 'user' as 'member'
  // to match the Principal type (which uses 'member' | 'admin').
  const rawRole = (session.user as { role?: string }).role ?? "user";
  const role = rawRole === "admin" ? "admin" : ("member" as const);
  const isAdmin = role === "admin";

  const principal: Principal = {
    email: session.user.email,
    display_name: session.user.name || null,
    role,
    is_admin: isAdmin,
    has_claude_token: hasClaudeToken ?? false,
    // better-auth uses a session cookie — sign-out is always meaningful.
    can_sign_out: true,
    // Role is set directly by the admin plugin (not via SCIM or IdP claim
    // in this deployment tier); leave role_source undefined (optional field).
  };

  const value: AuthState = {
    principal,
    isAdmin,
    refresh: () => void refetchToken(),
  };

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}

export function useAuth(): AuthState {
  const ctx = useContext(AuthContext);
  if (!ctx) {
    throw new Error("useAuth must be used within an AuthProvider");
  }
  return ctx;
}

/** Returns the current auth state, or null when signed out.
 * Used by App.tsx's InnerApp to pass nullable context to the router
 * (which handles the unauthenticated redirect in appLayoutRoute.beforeLoad). */
export function useOptionalAuth(): AuthState | null {
  return useContext(AuthContext);
}

export function useIsAdmin(): boolean {
  return useAuth().isAdmin;
}

// ---- Sign-out helper (replaces the old coordinator POST /auth/logout) ----
//
// Used by user-menu.tsx. Hard-navigates to /login so the session query
// re-initialises from a clean state and the router lands on the login page.
export async function signOut(): Promise<void> {
  await authClient.signOut();
  window.location.assign("/login");
}

// ---- Auth state screens --------------------------------------------------

function AuthStage({ children }: { children: ReactNode }) {
  return (
    <div className="grid min-h-svh place-items-center bg-background p-8 text-foreground">
      {children}
    </div>
  );
}

function BootScreen() {
  return (
    <AuthStage>
      <div className="flex flex-col items-center gap-3 text-center">
        <EngramMark size={72} mode="loop" />
        <p className="text-sm italic text-muted-foreground">authenticating…</p>
      </div>
    </AuthStage>
  );
}

// Shown when authClient.useSession() returns an error (orchestrator unreachable).
// Exported so tests can import the component directly.
export function AuthErrorScreen({ message, onRetry }: { message?: string; onRetry: () => void }) {
  return (
    <AuthStage>
      <Card className="w-full max-w-md">
        <CardContent className="flex flex-col items-center gap-4 py-8 text-center">
          <EngramMark size={72} mode="static" />
          <p className="text-base font-medium">Could not reach the coordinator — retrying…</p>
          {message && <p className="text-sm text-muted-foreground">{message}</p>}
          <Button variant="outline" onClick={onRetry}>
            Retry now
          </Button>
        </CardContent>
      </Card>
    </AuthStage>
  );
}
