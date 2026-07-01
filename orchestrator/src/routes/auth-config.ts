/**
 * GET /api/v1/auth-config — public auth posture for the SPA login page.
 *
 * Unauthenticated by design: the web Login page renders BEFORE any session
 * exists, so it must learn which sign-in doors are open without a credential.
 * The only thing exposed is which auth mechanisms are enabled — never any
 * secret, email, or assertion content.
 *
 * Posture is derived live from `config` (read per-request, not captured at
 * import) so it always reflects the running deployment:
 *   - `passwordAuth`: email/password sign-in is available. FALSE behind GCP IAP
 *     (IAP_AUDIENCES set) — IAP is then the sole identity source and the
 *     better-auth password door is disabled (see better-auth.ts), so the Login
 *     page must not render a form the server would reject.
 *   - `signup`: public sign-up is available. Tracks `passwordAuth` — there is no
 *     password sign-up door when password auth itself is off.
 *
 * Behind IAP the SPA never actually reaches /login (the bridge authenticates
 * every request), so this mainly drives the dev / self-hosted (no-IAP) login
 * UI; it exists so the page is honest in every posture rather than hard-coding
 * a dev-only assumption.
 */

import { Hono } from "hono";
import { config } from "../config.ts";

const authConfigRoute = new Hono();

authConfigRoute.get("/api/v1/auth-config", (c) => {
  const passwordAuth = config.iapAudiences.length === 0;
  return c.json({
    passwordAuth,
    // No public sign-up without a password door.
    signup: passwordAuth,
  });
});

export default authConfigRoute;
