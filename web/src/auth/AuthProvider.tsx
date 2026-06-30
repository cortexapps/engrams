// ADR 0051 §5 / Task 22 — AuthProvider recomposed on better-auth.
//
// The external interface (AuthState: principal, isAdmin, ability) — every
// consumer (UserChip, ProfilePanel, NewSessionDialog, Members, RequireAdmin,
// router.ts guards) reads from it.
//
// Internal recomposition:
//   - authn + role:  authClient.useSession() — reads the better-auth session
//     cookie (HttpOnly, same-origin). The admin plugin writes `role` onto the
//     user object; `role === 'admin'` drives isAdmin.
//   - Per-user harness credentials live OUTSIDE the principal now (ADR 0063):
//     the settings page reads them via useHarnessEnv (GET /me/harness-env), so
//     the provider no longer fetches token presence at all.
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
//   The Principal type (lib/types.ts) mirrors the coordinator's GET /me
//   response shape. We synthesise an equivalent object from the better-auth session
//   so that every consumer reads `principal.email`, `principal.role`,
//   `principal.is_admin`, `principal.display_name`, and `principal.can_sign_out`.

import { createContext, useContext, useMemo, type ReactNode } from "react";
import { authClient } from "@/lib/auth-client";
import type { Principal } from "../lib/types";
import { abilityFor, type AppAbility } from "@/lib/ability";
import { EngramMark } from "../components/EngramMark";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";

export interface AuthState {
  /** Always present for children — the provider only renders them once the
   * principal has resolved. */
  principal: Principal;
  isAdmin: boolean;
  /** CASL ability instance for the current user (ADR 0051 §6). */
  ability: AppAbility;
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

  // ADR 0051 §6: derive role/id for ability BEFORE any conditional returns so
  // that useMemo is always called (Rules of Hooks — no hooks after early returns).
  // When session is not yet resolved these are empty strings; the ability instance
  // is only used in the value object that is created further down (after the
  // guards), so the placeholder never escapes to a consumer.
  const rawRole = (session?.user as { role?: string } | undefined)?.role ?? "user";
  const ability = useMemo(
    () => abilityFor({ id: session?.user.id ?? "", role: rawRole }),
    // session?.user.id and rawRole are the only deps that affect the ability shape.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [session?.user.id, rawRole],
  );

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

  // The admin plugin writes role as 'admin' | 'user'. We treat 'user' as 'member'
  // to match the Principal type (which uses 'member' | 'admin').
  const role = rawRole === "admin" ? "admin" : ("member" as const);
  const isAdmin = role === "admin";

  const principal: Principal = {
    email: session.user.email,
    display_name: session.user.name || null,
    role,
    is_admin: isAdmin,
    // better-auth uses a session cookie — sign-out is always meaningful.
    can_sign_out: true,
    // Role is set directly by the admin plugin (not via SCIM or IdP claim
    // in this deployment tier); leave role_source undefined (optional field).
  };

  const value: AuthState = {
    principal,
    isAdmin,
    ability,
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

/** Returns the CASL AppAbility instance for the current user.
 * Use `ability.can('manage', 'all')` to gate admin-only UI. */
export function useAbility(): AppAbility {
  return useAuth().ability;
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
