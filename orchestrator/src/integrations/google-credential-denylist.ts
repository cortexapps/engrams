/**
 * Google credential-minting surfaces an administrator can never select as a
 * connection endpoint (ADR 0109).
 *
 * The table itself lives with the egress proxy, which is the enforcement point:
 * `crates/engram-egress-proxy/policy/google-credential-denylist.json`. Both sides
 * read that one file — the proxy through `include_str!`, this module through a
 * JSON import — so the two lists cannot drift apart. They already had: this
 * validator knew four hosts, the proxy knew five, and neither knew the mutual-TLS
 * twins (`sts.mtls.googleapis.com`), which made the exact-host list a complete
 * bypass.
 *
 * `docker/orchestrator.Dockerfile` copies the JSON into the runtime image at the
 * same relative path, so this import resolves the same way in both places.
 */

import denylist from "../../../crates/engram-egress-proxy/policy/google-credential-denylist.json" with {
  type: "json",
};

/**
 * Normalize a host for matching: lower case, no trailing dot, and no `.mtls`
 * label. Kept byte-for-byte equivalent to `normalize_host` in
 * `crates/engram-egress-proxy/src/google_denylist.rs`.
 */
export function normalizeGoogleHost(host: string): string {
  const lowered = host.replace(/\.+$/, "").toLowerCase();
  const mtls = lowered.indexOf(".mtls.");
  return mtls === -1 ? lowered : `${lowered.slice(0, mtls)}.${lowered.slice(mtls + ".mtls.".length)}`;
}

function hostMatches(pattern: string, host: string): boolean {
  if (!pattern.startsWith("*.")) return host === pattern;
  const suffix = pattern.slice(2);
  return host.endsWith(suffix) && host.length > suffix.length;
}

/** Is this host a credential-exchange surface the guest must never reach? */
export function isDeniedGoogleHost(host: string): boolean {
  const normalized = normalizeGoogleHost(host);
  return denylist.denied_hosts.some((pattern) => hostMatches(pattern, normalized));
}

/** Every denied host, for tests and operator-facing messages. */
export const deniedGoogleHosts: readonly string[] = denylist.denied_hosts;
