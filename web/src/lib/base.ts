/**
 * Base URLs for Engram API routes.
 *
 * Every coordinator and orchestrator HTTP route lives under `/api/v1`, and
 * the Connect RPCs under `/rpc`. Exported here (single source of truth) so
 * SSE, WS, and artifact URL builders don't need to import from the
 * now-deleted api.ts.
 *
 * ## Split-host deployments (API_ORIGIN)
 *
 * By default the SPA calls the API same-origin (dev: the Vite proxy →
 * :8787 orchestrator; prod: nginx / the ingress). A deployment that fronts
 * the SPA with an authenticating proxy (e.g. GCP IAP) serves the machine
 * surface from a SECOND hostname instead — proxies of that kind break
 * WebSocket upgrades and cap SSE lifetimes, so the streaming legs must
 * bypass them. Such a deployment injects the api origin at runtime via
 * `/engram-config.js` (the web chart renders it from `web.apiBaseUrl`;
 * the default file in public/ leaves it empty).
 *
 * When API_ORIGIN is set, every URL built here is absolute, and requests
 * must carry credentials cross-origin (API_CREDENTIALS below) — the
 * orchestrator's CORS gate admits exactly the origins in its
 * TRUSTED_ORIGINS list. The session cookie must be scoped to the shared
 * parent domain (ORCHESTRATOR_SESSION_COOKIE_DOMAIN) for the browser to
 * attach it.
 *
 * The SPA owns the root path namespace so deep-links like `/sessions/:id`
 * never collide with `/api/v1/sessions/:id`.
 */

declare global {
  interface Window {
    /** Injected by /engram-config.js before the bundle loads. */
    __ENGRAM_API_ORIGIN__?: string;
  }
}

/** The api host's origin (`https://api.example.com`), or "" for
 * same-origin deployments. Trailing slashes are tolerated in the
 * injected value. */
export const API_ORIGIN: string =
  typeof window !== "undefined" && window.__ENGRAM_API_ORIGIN__
    ? window.__ENGRAM_API_ORIGIN__.replace(/\/+$/, "")
    : "";

export const API_BASE = `${API_ORIGIN}/api/v1`;

/** Base for the Connect transport (App.tsx). */
export const RPC_BASE = `${API_ORIGIN}/rpc`;

/** Credentials mode for fetch()/EventSource against API_BASE. Cookies are
 * not attached cross-origin under the default "same-origin" mode, so a
 * split-host deployment needs "include" (and the server echoes
 * Access-Control-Allow-Credentials). Kept at "same-origin" otherwise —
 * no behavior change for single-host deployments. */
export const API_CREDENTIALS: RequestCredentials = API_ORIGIN ? "include" : "same-origin";

/** Build a WebSocket URL for a path under `/api/v1`. Handles the
 * ws/wss scheme for both the same-origin and split-host layouts. */
export function apiWsUrl(pathUnderApiBase: string): string {
  if (API_ORIGIN) {
    return `${API_ORIGIN.replace(/^http/, "ws")}/api/v1${pathUnderApiBase}`;
  }
  const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
  return `${protocol}//${window.location.host}/api/v1${pathUnderApiBase}`;
}
