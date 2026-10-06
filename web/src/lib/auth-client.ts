import { createAuthClient } from "better-auth/react";
import { adminClient } from "better-auth/client/plugins";

// Same-origin: the vite proxy (dev) / fronting LB (prod) routes /api/auth
// to the orchestrator. Don't set a cross-origin baseURL — cookie scoping
// requires same-origin delivery so the HttpOnly session cookie rides every
// subsequent same-origin request automatically.
export const authClient = createAuthClient({ plugins: [adminClient()] });

/**
 * Start the `oauth` sign-in door: ask the server for the identity provider's
 * authorization URL. The caller navigates the browser there.
 *
 * A plain call to the server's generic-OAuth endpoint, rather than a client
 * plugin: the endpoint's contract (`{ url }`) is all the page needs, and it
 * keeps the page independent of which client plugins a library version ships.
 */
export async function startOAuthSignIn(body: {
  providerId: string;
  callbackURL: string;
  errorCallbackURL: string;
}): Promise<{ url: string } | { error: string }> {
  const res = await authClient.$fetch<{ url?: string }>("/sign-in/oauth2", {
    method: "POST",
    body,
  });
  if (res.error || !res.data?.url) {
    return { error: res.error?.message ?? "Sign-in failed" };
  }
  return { url: res.data.url };
}
