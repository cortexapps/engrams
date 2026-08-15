// Login page — email + password sign-in via better-auth (ADR 0051 §5 / Task 22).
//
// Auth posture is server-driven via GET /api/v1/auth-config:
//   - passwordAuth=false (orchestrator behind GCP IAP): IAP is the sole identity
//     source — the better-auth password door is disabled server-side, so we render
//     an SSO notice instead of a form the server would reject. (Behind IAP the SPA
//     normally never reaches /login at all; this is the honest fallback.)
//   - passwordAuth=true (dev / self-hosted): the email+password form is shown.
//     `signup` then gates the open-registration toggle.
//
// On success: hard-navigates to / via window.location.assign so AuthProvider's
// session query re-initialises from scratch and the router re-evaluates the
// appLayoutRoute.beforeLoad guard with the fresh session.

import { useEffect, useState } from "react";
import { authClient } from "@/lib/auth-client";
import { API_BASE } from "@/lib/base";
import { safeNextUrl, DEFAULT_AFTER_LOGIN } from "@/lib/next-url";
import { EngramMark } from "@/components/EngramMark";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";

interface AuthConfig {
  passwordAuth: boolean;
  signup: boolean;
  /** ADR 0118: needed to validate a `?next=` pointing at a session app. */
  previewBaseDomain?: string;
}

export function Login() {
  // Auth posture from the server. `undefined` while loading; on fetch failure we
  // fall back to the password form (dev default) so a transient error doesn't
  // lock everyone out of the only login path.
  const [authConfig, setAuthConfig] = useState<AuthConfig | undefined>(undefined);
  const [mode, setMode] = useState<"sign-in" | "sign-up">("sign-in");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [name, setName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const res = await fetch(`${API_BASE}/auth-config`, { credentials: "same-origin" });
        if (!res.ok) throw new Error(`auth-config ${res.status}`);
        const cfg = (await res.json()) as AuthConfig;
        if (!cancelled) setAuthConfig(cfg);
      } catch {
        // Fall back to the password door so a transient fetch error never
        // strands the dev/self-hosted login.
        if (!cancelled) setAuthConfig({ passwordAuth: true, signup: true });
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  // ADR 0118: behind IAP the bridge has already minted a session by the time
  // this page renders, so a `?next=` arrival is a round trip that is already
  // complete — send them on rather than showing an SSO notice they cannot act
  // on. `authConfig` gates it so the destination is validated against the live
  // preview domain, not a guess.
  useEffect(() => {
    if (!authConfig) return;
    const next = safeNextUrl(window.location.search, authConfig.previewBaseDomain);
    if (next === DEFAULT_AFTER_LOGIN) return;
    let cancelled = false;
    void (async () => {
      const { data } = await authClient.getSession();
      if (!cancelled && data?.session) window.location.assign(next);
    })();
    return () => {
      cancelled = true;
    };
  }, [authConfig]);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    setLoading(true);

    try {
      if (mode === "sign-in") {
        const res = await authClient.signIn.email({ email, password });
        if (res.error) {
          setError(res.error.message ?? "Sign-in failed");
          return;
        }
      } else {
        const res = await authClient.signUp.email({ email, password, name });
        if (res.error) {
          setError(res.error.message ?? "Sign-up failed");
          return;
        }
      }
      // Hard-navigate so AuthProvider's session query re-initialises and the
      // router re-evaluates the appLayoutRoute.beforeLoad guard with the freshly
      // issued session cookie. ADR 0118: a validated `?next=` returns the user
      // to the session app they were trying to open.
      window.location.assign(safeNextUrl(window.location.search, authConfig?.previewBaseDomain));
    } catch (err) {
      setError(err instanceof Error ? err.message : "Authentication failed");
    } finally {
      setLoading(false);
    }
  };

  const passwordAuth = authConfig?.passwordAuth ?? true;
  const signupEnabled = authConfig?.signup ?? false;

  return (
    <div className="grid min-h-svh place-items-center bg-background p-8 text-foreground">
      <div className="flex w-full max-w-sm flex-col items-center gap-6">
        {/* Identity mark */}
        <div className="flex flex-col items-center gap-2">
          <EngramMark size={48} mode="static" />
          <span className="text-base font-semibold tracking-tight">engrams</span>
        </div>

        {authConfig === undefined ? (
          // Posture still loading — keep the chrome stable, no flash of a form.
          <Card className="w-full">
            <CardContent className="py-8 text-center text-sm text-muted-foreground">
              Loading…
            </CardContent>
          </Card>
        ) : !passwordAuth ? (
          // Single sign-on deployment: no password door. The user is normally
          // signed in automatically before reaching this page.
          <Card className="w-full">
            <CardHeader className="space-y-1 pb-4">
              <CardTitle className="text-lg">Single sign-on</CardTitle>
              <CardDescription>
                This deployment authenticates through your identity provider — you'll be signed in
                automatically. If you're seeing this, try reloading.
              </CardDescription>
            </CardHeader>
            <CardContent>
              <Button className="w-full" onClick={() => window.location.assign("/")}>
                Reload
              </Button>
            </CardContent>
          </Card>
        ) : (
          <Card className="w-full">
            <CardHeader className="space-y-1 pb-4">
              <CardTitle className="text-lg">
                {mode === "sign-in" ? "Sign in" : "Create account"}
              </CardTitle>
              {mode === "sign-up" && (
                <CardDescription>
                  Dev-only — production gates registration on an allowlist.
                </CardDescription>
              )}
            </CardHeader>
            <CardContent>
              <form onSubmit={(e) => void handleSubmit(e)} className="space-y-4">
                {mode === "sign-up" && (
                  <div className="space-y-1.5">
                    <Label htmlFor="name">Name</Label>
                    <Input
                      id="name"
                      type="text"
                      autoComplete="name"
                      placeholder="Your name"
                      value={name}
                      onChange={(e) => setName(e.target.value)}
                      disabled={loading}
                    />
                  </div>
                )}
                <div className="space-y-1.5">
                  <Label htmlFor="email">Email</Label>
                  <Input
                    id="email"
                    type="email"
                    autoComplete={mode === "sign-in" ? "username" : "email"}
                    placeholder="you@example.com"
                    value={email}
                    onChange={(e) => setEmail(e.target.value)}
                    required
                    disabled={loading}
                  />
                </div>
                <div className="space-y-1.5">
                  <Label htmlFor="password">Password</Label>
                  <Input
                    id="password"
                    type="password"
                    autoComplete={mode === "sign-in" ? "current-password" : "new-password"}
                    placeholder="••••••••"
                    value={password}
                    onChange={(e) => setPassword(e.target.value)}
                    required
                    disabled={loading}
                    minLength={mode === "sign-up" ? 8 : undefined}
                  />
                </div>

                {error && (
                  <p className="rounded-md bg-destructive/10 px-3 py-2 text-sm text-destructive">
                    {error}
                  </p>
                )}

                <Button type="submit" className="w-full" disabled={loading}>
                  {loading
                    ? mode === "sign-in"
                      ? "Signing in…"
                      : "Creating account…"
                    : mode === "sign-in"
                      ? "Sign in"
                      : "Create account"}
                </Button>
              </form>

              {signupEnabled && (
                <p className="mt-4 text-center text-sm text-muted-foreground">
                  {mode === "sign-in" ? (
                    <>
                      No account?{" "}
                      <button
                        type="button"
                        onClick={() => {
                          setMode("sign-up");
                          setError(null);
                        }}
                        className="underline hover:text-foreground"
                      >
                        Sign up
                      </button>
                    </>
                  ) : (
                    <>
                      Already have an account?{" "}
                      <button
                        type="button"
                        onClick={() => {
                          setMode("sign-in");
                          setError(null);
                        }}
                        className="underline hover:text-foreground"
                      >
                        Sign in
                      </button>
                    </>
                  )}
                </p>
              )}
            </CardContent>
          </Card>
        )}
      </div>
    </div>
  );
}
