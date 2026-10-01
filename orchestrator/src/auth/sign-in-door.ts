/**
 * The sign-in door: the better-auth options that config.authMode selects.
 *
 * A deployment has ONE human sign-in door (ORCHESTRATOR_AUTH_MODE), and the
 * doors are exclusive:
 *
 *   `oauth`    — the genericOAuth plugin against the deployment's OIDC
 *                provider, behind the allowlist gate (oauth-gate.ts).
 *                emailAndPassword is off.
 *   `iap`      — neither door here: the IAP bridge (iap-bridge.ts) mints the
 *                sessions. emailAndPassword is off.
 *   `password` — emailAndPassword; sign-up follows config.passwordSignup.
 *
 * Kept apart from better-auth.ts — which builds its instance from the config
 * singleton at import — so a test can build a real better-auth instance for
 * each mode (see __tests__/oauth-login.test.ts).
 *
 * ## The `oauth` provider
 *
 * Any provider that publishes an OIDC discovery document works — Google,
 * Okta, Auth0, Keycloak, Cognito, GitLab. The deployment sets the issuer and a
 * client id + secret, and registers this redirect URI with the provider:
 *
 *   ${ORCHESTRATOR_PUBLIC_URL}/api/auth/oauth2/callback/<providerId>
 */

import type { BetterAuthOptions } from "better-auth";
import { APIError } from "better-auth/api";
import { genericOAuth, type GenericOAuthConfig } from "better-auth/plugins/generic-oauth";
import type { Config, OAuthConfig } from "../config.ts";
import { checkOAuthProfile, isGoogleIssuer } from "./oauth-gate.ts";

/**
 * Build the genericOAuth provider config.
 *
 * `loginUrl` is the SPA login page. A refused sign-in is redirected there with
 * `?error=<OAuthRejection>` so the person sees why, in the app, instead of a
 * JSON error on the callback URL.
 */
export function oauthProviderConfig(oauth: OAuthConfig, loginUrl: string): GenericOAuthConfig {
  const google = isGoogleIssuer(oauth.issuer);
  // A UX hint only: with one allowed Workspace domain, Google's account
  // chooser lists that domain's accounts. It is NOT the access check — a
  // caller can drop the parameter; the gate below reads the signed-in
  // account's own claims.
  const onlyDomain =
    oauth.allowlist.domains.length === 1 && oauth.allowlist.domains[0] !== "*"
      ? oauth.allowlist.domains[0]
      : undefined;

  return {
    providerId: oauth.providerId,
    clientId: oauth.clientId,
    clientSecret: oauth.clientSecret,
    discoveryUrl: `${oauth.issuer}/.well-known/openid-configuration`,
    scopes: oauth.scopes,
    // PKCE binds the authorization code to the browser that started the flow.
    // Every provider listed above accepts it from a confidential client.
    pkce: true,
    ...(google
      ? {
          // Always show Google's account chooser. Without it a person whose
          // browser holds one personal Google session is refused by the gate,
          // presses the button again, and is silently signed in as the same
          // account — with no way to pick another.
          prompt: "select_account" as const,
          ...(onlyDomain ? { authorizationUrlParams: { hd: onlyDomain } } : {}),
        }
      : {}),
    // Runs on EVERY sign-in, before better-auth looks up or creates the user:
    // the gate therefore covers a first sign-in, a returning user, and a user
    // whose address has since left the allowlist.
    mapProfileToUser(profile) {
      const rejection = checkOAuthProfile(profile, oauth.issuer, oauth.allowlist);
      if (rejection) {
        // The same shape better-auth's own `ctx.redirect()` throws: a 302
        // that the router turns into the response.
        throw new APIError(
          "FOUND",
          undefined,
          new Headers({ location: `${loginUrl}?error=${rejection}` }),
        );
      }
      const email = String(profile["email"]).trim().toLowerCase();
      const name = typeof profile["name"] === "string" ? profile["name"].trim() : "";
      return {
        // The gate just required the provider's verified-email assertion.
        // Written as a boolean because some providers send the string "true".
        emailVerified: true,
        // better-auth refuses a sign-in with no name. A provider that omits it
        // (no `profile` scope) gets the mailbox name, as the IAP bridge does.
        name: name || (email.split("@")[0] ?? email),
      };
    },
  };
}

/** The slice of Config that selects the door. */
export type SignInDoorConfig = Pick<Config, "authMode" | "passwordSignup" | "oauth">;

/**
 * The better-auth options for the configured door. `plugins` holds the OAuth
 * plugin in `oauth` mode and is empty otherwise — so every
 * /api/auth/oauth2/* route is absent in the other two modes.
 */
export function signInDoor(
  cfg: SignInDoorConfig,
  loginUrl: string,
): {
  emailAndPassword: NonNullable<BetterAuthOptions["emailAndPassword"]>;
  plugins: ReturnType<typeof genericOAuth>[];
} {
  return {
    // Email/password is the door ONLY in `password` mode. In `oauth` and `iap`
    // mode the identity provider (or IAP) is the sole identity source, so a
    // parallel password door — a credential to phish or brute-force, and with
    // sign-up open, a way for anyone to make an account — is pure attack
    // surface and is disabled outright: sign-in and sign-up both answer 400.
    //
    // `disableSignUp` closes registration while existing accounts keep signing
    // in. An email is not verified on sign-up, so open registration means
    // anyone can claim any address; it is for dev and for a deployment that is
    // not reachable from the internet.
    emailAndPassword: {
      enabled: cfg.authMode === "password",
      disableSignUp: !cfg.passwordSignup,
    },
    plugins:
      cfg.authMode === "oauth" && cfg.oauth
        ? [genericOAuth({ config: [oauthProviderConfig(cfg.oauth, loginUrl)] })]
        : [],
  };
}
