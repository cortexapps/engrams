import { ConnectError } from "@connectrpc/connect";

/**
 * Extract a human-readable message from any thrown value, with first-class
 * handling for Connect/gRPC errors.
 *
 * Motivating incident: a registry-auth failure on enable-image surfaced in
 * the UI as the opaque `ConnectError: [internal] HTTP 400` because the
 * display path used `String(err)`. For a ConnectError, `String(err)` keeps
 * the `[code]` prefix and the class name; we want the clean upstream
 * message the coordinator put in `Status::message()` — e.g.
 * `registry pull for … failed: … Not authorized`.
 *
 * `ConnectError.from(err).rawMessage` is the message WITHOUT the `[code]`
 * prefix that `ConnectError.toString()` prepends. For non-Connect errors we
 * fall back to `.message` / `String(err)`.
 */
export function errorMessage(err: unknown): string {
  if (err instanceof ConnectError) {
    return err.rawMessage;
  }
  // Some transports wrap the ConnectError; `from` unwraps known shapes.
  const ce = ConnectError.from(err);
  // `ConnectError.from` of a plain Error keeps its message but defaults the
  // code to "unknown"; only treat it as a Connect error if the input truly
  // was one (handled above) — otherwise prefer the native message.
  if (err instanceof Error) {
    return err.message || ce.rawMessage || String(err);
  }
  return ce.rawMessage || String(err);
}
