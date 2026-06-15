// Login page — email + password sign-in via better-auth (ADR 0051 §5 / Task 22).
//
// Production decision (recorded here per the plan):
//   - Public sign-up is DEV-ONLY. The sign-up section below is currently
//     always enabled (matches the orchestrator's emailAndPassword.enabled posture
//     which is also dev-only for now).
//   - Before production: disable sign-up here AND in better-auth config, or gate
//     on an admin-managed allowlist (Task 30/31). Do NOT ship open registration
//     to a public-facing deployment.
//
// On success: hard-navigates to / via window.location.assign so AuthProvider's
// session query re-initialises from scratch and the router re-evaluates the
// appLayoutRoute.beforeLoad guard with the fresh session.

import { useState } from "react";
import { authClient } from "@/lib/auth-client";
import { EngramMark } from "@/components/EngramMark";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";

// Dev-only: show sign-up toggle. In production, set this to false and gate
// registration on an admin allowlist before enabling open sign-up.
// TODO Task 30: wire to VITE_ENABLE_SIGNUP env var once prod posture is decided.
const ENABLE_SIGNUP = true;

export function Login() {
  const [mode, setMode] = useState<"sign-in" | "sign-up">("sign-in");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [name, setName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

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
      // Hard-navigate to / so AuthProvider's session query re-initialises and
      // the router re-evaluates the appLayoutRoute.beforeLoad guard with the
      // freshly issued session cookie.
      window.location.assign("/");
    } catch (err) {
      setError(err instanceof Error ? err.message : "Authentication failed");
    } finally {
      setLoading(false);
    }
  };

  return (
    <div className="grid min-h-svh place-items-center bg-background p-8 text-foreground">
      <div className="flex w-full max-w-sm flex-col items-center gap-6">
        {/* Identity mark */}
        <div className="flex flex-col items-center gap-2">
          <EngramMark size={48} mode="static" />
          <span className="font-display text-sm font-semibold tracking-widest text-muted-foreground uppercase">
            engrams
          </span>
        </div>

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

            {ENABLE_SIGNUP && (
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
      </div>
    </div>
  );
}
