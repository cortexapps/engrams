/**
 * Cross-origin gate for the split-host deployment (app host + api host).
 *
 * When the SPA is served from one origin (e.g. https://engrams.example.com,
 * behind an authenticating proxy) and the orchestrator's machine surface from
 * another (e.g. https://api.engrams.example.com), every browser call — the
 * Connect RPCs, the /api/v1 fetches, the EventSource legs — is cross-origin
 * and rides `credentials: "include"`. This module answers preflights and
 * stamps the response headers that let those calls through, for exactly the
 * origins in TRUSTED_ORIGINS (config.trustedOrigins). Everything else gets no
 * CORS headers, which for a credentialed cross-origin request means the
 * browser refuses to deliver the response — fail closed by omission.
 *
 * Placement: BEFORE the IAP bridge in the server's request path. A preflight
 * OPTIONS carries no cookie and no IAP assertion by spec, so a bridge that
 * fails closed would 401 it and no cross-origin call could ever start. The
 * preflight response itself carries no data, so answering it pre-auth is
 * safe.
 *
 * Same-origin deployments (dev, the OSS single-host default) are untouched:
 * same-origin requests either carry no Origin header, or carry one equal to
 * the request host, and browsers ignore CORS headers on same-origin
 * responses anyway. The one behavioral addition — answering OPTIONS with
 * Access-Control-Request-Method pre-auth — only fires for genuine
 * preflights.
 *
 * WebSockets are NOT governed by CORS; see `wsOriginAllowed` below, which the
 * upgrade path uses to refuse cross-site handshakes (a page on any origin
 * can open a WS with the visitor's cookie attached — with the session cookie
 * widened to the parent domain for the split-host layout, a guest-authored
 * preview page could otherwise drive an authenticated socket).
 */

import type { IncomingMessage, ServerResponse } from "node:http";
import { config } from "../config.ts";

/** Headers a credentialed cross-origin response needs. Only stamped when the
 * Origin is trusted. `Vary: Origin` keeps shared caches from serving one
 * origin's approval to another. */
function stampCorsHeaders(res: ServerResponse, origin: string): void {
  res.setHeader("Access-Control-Allow-Origin", origin);
  res.setHeader("Access-Control-Allow-Credentials", "true");
  res.setHeader("Vary", "Origin");
}

/**
 * Apply the CORS gate to one request. Returns `true` when the request was a
 * preflight and has been fully answered (the caller must not continue the
 * dispatch), `false` to continue.
 */
export function applyCors(
  req: IncomingMessage,
  res: ServerResponse,
  trustedOrigins: string[] = config.trustedOrigins,
): boolean {
  const origin = req.headers.origin;
  if (!origin) return false;

  const trusted = trustedOrigins.includes(origin);
  if (trusted) stampCorsHeaders(res, origin);

  // A preflight is OPTIONS + Access-Control-Request-Method. Answer it here —
  // trusted or not — so it never reaches the fail-closed auth path: for a
  // trusted origin the 204 carries the approval headers; for anything else
  // it carries none and the browser blocks the real request.
  const requestMethod = req.headers["access-control-request-method"];
  if (req.method === "OPTIONS" && typeof requestMethod === "string") {
    if (trusted) {
      res.setHeader("Access-Control-Allow-Methods", "GET, POST, PUT, PATCH, DELETE, OPTIONS");
      // Echo whatever the request wants to send (connect-web sends
      // connect-protocol-version etc.; better-auth sends content-type).
      const requestHeaders = req.headers["access-control-request-headers"];
      res.setHeader(
        "Access-Control-Allow-Headers",
        typeof requestHeaders === "string" && requestHeaders.length > 0
          ? requestHeaders
          : "content-type",
      );
      res.setHeader("Access-Control-Max-Age", "600");
    }
    res.statusCode = 204;
    res.end();
    return true;
  }

  return false;
}

/**
 * WebSocket handshake origin policy. Allowed when:
 *  - there is no Origin header (non-browser clients: the CLI, server-side
 *    tools — their auth is the cookie/API-key guard on the route), or
 *  - the Origin's host equals the request's Host (same-origin), or
 *  - the Origin is in TRUSTED_ORIGINS (the split-host SPA).
 *
 * Anything else is a cross-site handshake carrying the visitor's cookie —
 * refuse before the route guard ever runs.
 */
export function wsOriginAllowed(
  req: IncomingMessage,
  trustedOrigins: string[] = config.trustedOrigins,
): boolean {
  const origin = req.headers.origin;
  if (!origin) return true;
  if (trustedOrigins.includes(origin)) return true;
  try {
    return new URL(origin).host === req.headers.host;
  } catch {
    return false; // unparseable Origin — refuse
  }
}
