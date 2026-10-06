// Login page — the deployment's ONE sign-in door, via better-auth.
//
// The door is server-driven (GET /api/v1/auth-config → `mode`, lib/sign-in.ts):
//   - oauth:    a "Continue with <provider>" button. The identity provider
//               signs the person in and the browser returns here (or to the
//               app). A refused sign-in returns with `?error=<code>`.
//   - password: the email + password form. `signup` then gates the
//               open-registration toggle.
//   - iap:      no door — the proxy in front of the app is the sole identity
//               source and has already signed the person in. (Behind IAP the
//               SPA normally never reaches /login; this is the honest fallback.)
//
// On success: hard-navigates via window.location.assign so AuthProvider's
// session query re-initialises from scratch and the router re-evaluates the
// appLayoutRoute.beforeLoad guard with the fresh session.

import { useCallback, useEffect, useState } from "react";
import { authClient, startOAuthSignIn } from "@/lib/auth-client";
import { API_BASE } from "@/lib/base";
import { safeNextUrl, hasNextParam } from "@/lib/next-url";
import { oauthErrorMessage, oauthReturnUrls, type AuthConfig } from "@/lib/sign-in";
import { EngramMark } from "@/components/EngramMark";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { SkeletonRows } from "@/components/skeleton-rows";

export function Login() {
  // Auth posture from the server: `undefined` while loading, `"unavailable"`
  // when the fetch failed. A failure is shown as a failure — guessing a door
  // would render a form (or a button) that this deployment may not have.
  const [authConfig, setAuthConfig] = useState<AuthConfig | "unavailable" | undefined>(undefined);
  const [mode, setMode] = useState<"sign-in" | "sign-up">("sign-in");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [name, setName] = useState("");
  // A refused OAuth round trip returns here with `?error=<code>`.
  const [error, setError] = useState<string | null>(() =>
    oauthErrorMessage(window.location.search),
  );
  const [loading, setLoading] = useState(false);

  const loadAuthConfig = useCallback(async (isCancelled: () => boolean = () => false) => {
    try {
      const res = await fetch(`${API_BASE}/auth-config`, { credentials: "include" });
      if (!res.ok) throw new Error(`auth-config ${res.status}`);
      const cfg = (await res.json()) as AuthConfig;
      if (!isCancelled()) setAuthConfig(cfg);
    } catch {
      if (!isCancelled()) setAuthConfig("unavailable");
    }
  }, []);

  useEffect(() => {
    let cancelled = false;
    void loadAuthConfig(() => cancelled);
    return () => {
      cancelled = true;
    };
  }, [loadAuthConfig]);

  const posture = typeof authConfig === "object" ? authConfig : undefined;

  // ADR 0118: a `?next=` arrival with a session already in place is a round
  // trip that is complete — the OAuth provider just returned the person here,
  // or (behind IAP) the bridge minted the session before this page rendered.
  // Send them on. `posture` gates it so the destination is validated against
  // the live preview domain, not a guess.
  useEffect(() => {
    if (!posture) return;
    // Only act when a `next` actually arrived: without one, an ordinary visitor
    // to /login must keep seeing the door (and the router's beforeLoad has
    // already sent an authenticated one to the app).
    if (!hasNextParam(window.location.search)) return;
    let cancelled = false;
    void (async () => {
      const { data } = await authClient.getSession();
      if (cancelled || !data?.session) return;
      // safeNextUrl falls back to DEFAULT_AFTER_LOGIN, so an unusable `next`
      // lands on the dashboard rather than leaving a signed-in user parked in
      // front of a login door they have no reason to use.
      window.location.assign(safeNextUrl(window.location.search, posture.previewBaseDomain));
    })();
    return () => {
      cancelled = true;
    };
  }, [posture]);

  const handleOAuth = async () => {
    if (!posture?.oauth) return;
    setError(null);
    setLoading(true);
    try {
      const res = await startOAuthSignIn({
        providerId: posture.oauth.providerId,
        ...oauthReturnUrls(window.location.search, window.location.origin),
      });
      if ("error" in res) {
        setError(res.error);
        setLoading(false);
        return;
      }
      // Off to the identity provider. `loading` stays set: the page is on its
      // way out.
      window.location.assign(res.url);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Sign-in failed");
      setLoading(false);
    }
  };

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
      window.location.assign(safeNextUrl(window.location.search, posture?.previewBaseDomain));
    } catch (err) {
      setError(err instanceof Error ? err.message : "Authentication failed");
    } finally {
      setLoading(false);
    }
  };

  const signupEnabled = posture?.signup ?? false;

  const errorNotice = error && (
    <p role="alert" className="rounded-md bg-destructive/10 px-3 py-2 text-sm text-destructive">
      {error}
    </p>
  );

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
            <CardContent className="py-4">
              <SkeletonRows rows={2} />
            </CardContent>
          </Card>
        ) : !posture ? (
          <Card className="w-full">
            <CardHeader className="space-y-1 pb-4">
              <CardTitle className="text-lg">Sign-in is unavailable</CardTitle>
              <CardDescription>
                The sign-in options did not load. Check your connection, then try again.
              </CardDescription>
            </CardHeader>
            <CardContent>
              <Button
                className="w-full"
                onClick={() => {
                  setAuthConfig(undefined);
                  void loadAuthConfig();
                }}
              >
                Try again
              </Button>
            </CardContent>
          </Card>
        ) : posture.mode === "oauth" && posture.oauth ? (
          <Card className="w-full">
            <CardHeader className="space-y-1 pb-4">
              <CardTitle className="text-lg">Sign in</CardTitle>
              <CardDescription>
                Use your {posture.oauth.displayName} account to continue.
              </CardDescription>
            </CardHeader>
            <CardContent className="space-y-4">
              {errorNotice}
              <Button className="w-full" disabled={loading} onClick={() => void handleOAuth()}>
                {loading ? "Redirecting…" : `Continue with ${posture.oauth.displayName}`}
              </Button>
            </CardContent>
          </Card>
        ) : !posture.passwordAuth ? (
          // Identity-proxy deployment: no door on this page. The person is
          // normally signed in automatically before reaching it.
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

                {errorNotice}

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
