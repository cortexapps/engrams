// /device — the browser leg of `engrams auth login` (RFC 8628 device flow).
//
// The CLI prints a one-time user code and opens this page (the plugin's
// verification_uri, ORCHESTRATOR_DEVICE_VERIFICATION_URL). The user confirms
// the code here; on Approve the CLI's /api/auth/device/token poll succeeds
// and it mints its durable API key (ApiKeyService.CreateCliKey).
//
// Endpoint choreography (all same-origin better-auth routes, cookie-authed):
//   1. GET  /api/auth/device?user_code=… — validates AND CLAIMS the grant for
//      this session (stamps userId on the row). Approve rejects an unclaimed
//      code, so this fetch must succeed before Approve/Deny render.
//   2. POST /api/auth/device/approve|deny { userCode }.
//
// The route sits under the authenticated app layout, so an anonymous visitor
// is bounced to /login first; codes are 8 chars from a 32-char uppercase
// charset (no dashes) — typed input is normalized before sending.

import { useCallback, useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";

/** Strip the display formatting (XXXX-XXXX, spaces) the CLI may show. */
function normalizeCode(raw: string): string {
  return raw.replace(/[\s-]/g, "").toUpperCase();
}

type Phase =
  | { kind: "enter" } // no/invalid code yet — show the input
  | { kind: "confirm"; code: string } // claimed, pending — show Approve/Deny
  | { kind: "approved" }
  | { kind: "denied" };

export function DeviceAuth({ initialCode }: { initialCode?: string }) {
  const [phase, setPhase] = useState<Phase>({ kind: "enter" });
  const [input, setInput] = useState(initialCode ?? "");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // Validate + claim the grant for this session (step 1 above).
  const claim = useCallback(async (raw: string) => {
    const code = normalizeCode(raw);
    if (!code) return;
    setBusy(true);
    setError(null);
    try {
      const res = await fetch(`/api/auth/device?user_code=${encodeURIComponent(code)}`, {
        credentials: "same-origin",
      });
      const body = (await res.json().catch(() => ({}))) as {
        status?: string;
        error_description?: string;
      };
      if (!res.ok) {
        setError(body.error_description ?? "That code is not valid.");
        return;
      }
      if (body.status !== "pending") {
        setError("That code has already been used — run `engrams auth login` again.");
        return;
      }
      setPhase({ kind: "confirm", code });
    } catch {
      setError("Could not reach the server — try again.");
    } finally {
      setBusy(false);
    }
  }, []);

  // A CLI-opened link arrives with ?user_code=… — claim it immediately.
  useEffect(() => {
    if (initialCode) void claim(initialCode);
  }, [initialCode, claim]);

  const decide = async (code: string, verb: "approve" | "deny") => {
    setBusy(true);
    setError(null);
    try {
      const res = await fetch(`/api/auth/device/${verb}`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        credentials: "same-origin",
        body: JSON.stringify({ userCode: code }),
      });
      if (!res.ok) {
        const body = (await res.json().catch(() => ({}))) as { error_description?: string };
        setError(body.error_description ?? `Could not ${verb} the code — try again.`);
        return;
      }
      setPhase({ kind: verb === "approve" ? "approved" : "denied" });
    } catch {
      setError("Could not reach the server — try again.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex min-h-full items-center justify-center p-6">
      <Card className="w-full max-w-md">
        {phase.kind === "enter" && (
          <>
            <CardHeader>
              <CardTitle>Connect the engrams CLI</CardTitle>
              <CardDescription>
                Enter the one-time code shown in your terminal by{" "}
                <code className="font-mono">engrams auth login</code>.
              </CardDescription>
            </CardHeader>
            <CardContent>
              <form
                className="space-y-4"
                onSubmit={(e) => {
                  e.preventDefault();
                  void claim(input);
                }}
              >
                <div className="space-y-2">
                  <Label htmlFor="user-code">One-time code</Label>
                  <Input
                    id="user-code"
                    value={input}
                    onChange={(e) => setInput(e.target.value)}
                    placeholder="XXXX-XXXX"
                    autoFocus
                    spellCheck={false}
                    autoCapitalize="characters"
                    className="font-mono tracking-widest"
                  />
                </div>
                {error && <p className="text-sm text-destructive">{error}</p>}
                <Button type="submit" className="w-full" disabled={busy || !input.trim()}>
                  {busy ? "Checking…" : "Continue"}
                </Button>
              </form>
            </CardContent>
          </>
        )}

        {phase.kind === "confirm" && (
          <>
            <CardHeader>
              <CardTitle>Authorize the engrams CLI?</CardTitle>
              <CardDescription>
                The CLI showing this code gets an API key that acts as you — same access, your name
                on every session it creates. Only approve a code from a terminal you are looking at
                right now.
              </CardDescription>
            </CardHeader>
            <CardContent className="space-y-4">
              <p className="text-center font-mono text-2xl tracking-[0.3em]">{phase.code}</p>
              {error && <p className="text-sm text-destructive">{error}</p>}
              <div className="flex gap-2">
                <Button
                  variant="outline"
                  className="flex-1"
                  disabled={busy}
                  onClick={() => void decide(phase.code, "deny")}
                >
                  Deny
                </Button>
                <Button
                  className="flex-1"
                  disabled={busy}
                  onClick={() => void decide(phase.code, "approve")}
                >
                  Approve
                </Button>
              </div>
            </CardContent>
          </>
        )}

        {phase.kind === "approved" && (
          <CardHeader>
            <CardTitle>CLI connected</CardTitle>
            <CardDescription>
              You can close this tab and return to your terminal — the CLI finishes logging in on
              its next poll. Revoke its key any time from Settings → API keys.
            </CardDescription>
          </CardHeader>
        )}

        {phase.kind === "denied" && (
          <CardHeader>
            <CardTitle>Request denied</CardTitle>
            <CardDescription>
              The CLI was not authorized. You can close this tab; nothing was granted.
            </CardDescription>
          </CardHeader>
        )}
      </Card>
    </div>
  );
}
