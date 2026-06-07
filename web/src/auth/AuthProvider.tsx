// ADR 0031 auth context. Owns the `GET /me` query and gates the app behind a
// resolved principal. A 401 is handled in the api client (hard redirect to
// /auth/login), so it never reaches here; this provider renders:
//   - boot screen while `/me` is in flight
//   - "not a member" screen for a 403 (authenticated but not provisioned)
//   - error screen for non-401/403 failures (coordinator down, etc.)

import { useQuery } from "@tanstack/react-query";
import { createContext, useContext, type ReactNode } from "react";
import { fetchMe, logout, NotMemberError } from "../api";
import type { Principal } from "../types";
import { EngramMark } from "../components/EngramMark";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";

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
  const {
    data: principal,
    isLoading,
    error,
    refetch,
  } = useQuery({
    queryKey: ["me"],
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
    throw new Error("useAuth must be used within an AuthProvider");
  }
  return ctx;
}

export function useIsAdmin(): boolean {
  return useAuth().isAdmin;
}

// ---- Auth state screens --------------------------------------------------
// Full-viewport, centered on the shadcn background, engram mark, quiet voice.

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

function AuthErrorScreen({ message, onRetry }: { message?: string; onRetry: () => void }) {
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

function NotMemberScreen({ email }: { email: string }) {
  return (
    <AuthStage>
      <Card className="w-full max-w-md">
        <CardContent className="flex flex-col items-center gap-3 py-8 text-center">
          <EngramMark size={72} mode="static" />
          <p className="text-base font-medium">
            You're signed in — but not yet a member of this deployment.
          </p>
          {email && <p className="font-mono text-sm text-muted-foreground">{email}</p>}
          <p className="text-sm text-muted-foreground">Ask an admin to add you, then reload.</p>
          <Button variant="ghost" onClick={() => void logout()}>
            Sign out
          </Button>
        </CardContent>
      </Card>
    </AuthStage>
  );
}
