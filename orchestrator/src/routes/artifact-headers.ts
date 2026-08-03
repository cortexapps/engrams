/**
 * Hardened response headers for artifact byte routes (ADR 0026).
 *
 * The upload gate accepts any file type; this header set — not the upload
 * path — is what makes serving attacker-controlled bytes safe:
 *   - `Content-Type` is the server-stored (coordinator-detected) type.
 *   - `X-Content-Type-Options: nosniff` stops MIME sniffing.
 *   - `Content-Security-Policy: sandbox …` gives the response a unique
 *     opaque origin even on direct navigation, so artifact HTML never runs
 *     on the app origin with credentials. HTML/SVG additionally get
 *     `allow-scripts` (and friends) so interactive artifacts work inside
 *     the sandbox.
 *   - Types with no inline rendering value are served as `attachment`.
 *   - `Cache-Control: private, no-store` — artifact access is revocable;
 *     never cache.
 *
 * Shared by the session-scoped route (routes/artifacts.ts) and the
 * cross-session artifact route.
 */

/** Sandbox for active document types: scripts run, but inside a unique
 * opaque origin with no cookies and no app-origin access. */
const ACTIVE_SANDBOX =
  "sandbox allow-scripts allow-forms allow-modals allow-popups allow-downloads";

/** Fully inert sandbox for everything else. */
const INERT_SANDBOX = "sandbox";

/** Document types that render inline with scripts enabled (sandboxed). */
const ACTIVE_INLINE_TYPES = new Set(["text/html", "image/svg+xml"]);

/** Non-active types that still render usefully inline in a browser. */
const INERT_INLINE_TYPES = new Set([
  "text/markdown",
  "text/plain",
  "text/csv",
  "application/json",
  "application/xml",
]);

function rendersInline(mediaType: string): boolean {
  return (
    ACTIVE_INLINE_TYPES.has(mediaType) ||
    INERT_INLINE_TYPES.has(mediaType) ||
    mediaType.startsWith("image/") ||
    mediaType.startsWith("video/") ||
    mediaType.startsWith("audio/")
  );
}

/**
 * Build the hardened header set for one artifact response.
 *
 * `fileName` (optional) is sanitised to printable ASCII for the
 * `Content-Disposition` filename parameter.
 */
export function artifactResponseHeaders(
  mediaType: string,
  fileName?: string,
): Record<string, string> {
  const mt = mediaType || "application/octet-stream";
  const disposition = rendersInline(mt) ? "inline" : "attachment";
  const csp = ACTIVE_INLINE_TYPES.has(mt) ? ACTIVE_SANDBOX : INERT_SANDBOX;

  const headers: Record<string, string> = {
    "Content-Type": mt,
    "X-Content-Type-Options": "nosniff",
    "Content-Security-Policy": csp,
    "Cache-Control": "private, no-store",
  };

  if (fileName) {
    // Keep the header value printable ASCII; quotes would break the framing.
    const safe = fileName.replace(/[^\x20-\x7E]/g, "_").replaceAll('"', "_");
    headers["Content-Disposition"] = `${disposition}; filename="${safe}"`;
  } else {
    headers["Content-Disposition"] = disposition;
  }

  return headers;
}
