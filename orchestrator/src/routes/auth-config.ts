/**
 * GET /api/v1/auth-config — public auth posture for the SPA login page.
 *
 * Unauthenticated by design: the web Login page renders BEFORE any session
 * exists, so it must learn which sign-in door is open without a credential.
 * The only thing exposed is which door the deployment uses — never a secret,
 * an email, an allowlist entry, or assertion content.
 *
 * Posture is derived live from `config` (read per-request, not captured at
 * import) so it always reflects the running deployment:
 *   - `mode`: the one sign-in door (config.authMode) — `oauth`, `iap`, or
 *     `password`. The page renders exactly that door.
 *   - `oauth`: present in `oauth` mode only. `providerId` is what the page
 *     passes to better-auth's sign-in call; `displayName` labels the button.
 *   - `passwordAuth`: email/password sign-in is available (`password` mode).
 *   - `signup`: open registration is available. Always false outside
 *     `password` mode, and false there when the deployment closed sign-up.
 *   - `previewBaseDomain`: ADR 0118. The preview handler sends an
 *     unauthenticated navigation here with `?next=<the app URL>`; the page needs
 *     this to decide whether that URL is one it may bounce back to. It is not a
 *     secret — it is in every preview URL a user has ever seen — and the page
 *     cannot safely hardcode it, since it differs per deployment.
 *
 * Behind IAP the SPA never actually reaches /login (the bridge authenticates
 * every request); the `iap` posture exists so the page is honest in every
 * mode rather than hard-coding an assumption.
 */

import { Hono } from "hono";
import { config } from "../config.ts";

const authConfigRoute = new Hono();

authConfigRoute.get("/api/v1/auth-config", (c) => {
  const passwordAuth = config.authMode === "password";
  return c.json({
    mode: config.authMode,
    ...(config.oauth
      ? {
          oauth: {
            providerId: config.oauth.providerId,
            displayName: config.oauth.displayName,
          },
        }
      : {}),
    passwordAuth,
    signup: passwordAuth && config.passwordSignup,
    previewBaseDomain: config.previewBaseDomain,
  });
});

export default authConfigRoute;
