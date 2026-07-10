/**
 * Credential + host resolution (the `gh` model).
 *
 * Precedence for the API key:
 *   1. ENGRAMS_API_KEY env var (CI, scripts) — never persisted.
 *   2. The stored entry for the active host in ~/.config/engrams/hosts.json
 *      (0600; written by `engrams auth login`).
 *   3. None → commands that need auth fail with a pointer to `auth login`.
 *
 * The active host URL: --url flag > ENGRAMS_URL env > DEFAULT_HOST. hosts.json
 * is keyed by normalized host URL so one machine can hold credentials for
 * prod and a local dev stack side by side.
 */

import { chmodSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

export const DEFAULT_HOST = "https://engrams.cortex.io";

/** The one OAuth client_id the orchestrator's device flow accepts. Must stay
 *  in lockstep with DEVICE_CLIENT_ID in orchestrator/src/auth/better-auth.ts. */
export const DEVICE_CLIENT_ID = "engrams-cli";

export interface HostCredential {
  /** The engk_… plaintext (only its hash exists server-side). */
  apiKey: string;
  /** The apikey row id — RevokeCliKey's handle at logout. */
  keyId: string;
  /** Owner email at login time (display only; the server re-resolves). */
  user?: string;
}

type HostsFile = Record<string, HostCredential>;

export function hostsPath(): string {
  return join(
    process.env["XDG_CONFIG_HOME"] ?? join(homedir(), ".config"),
    "engrams",
    "hosts.json",
  );
}

/** Strip a trailing slash so flag/env/stored spellings key identically. */
export function normalizeHost(url: string): string {
  return url.replace(/\/+$/, "");
}

export function resolveHost(flagUrl?: string): string {
  return normalizeHost(flagUrl || process.env["ENGRAMS_URL"] || DEFAULT_HOST);
}

function readHosts(): HostsFile {
  try {
    return JSON.parse(readFileSync(hostsPath(), "utf8")) as HostsFile;
  } catch {
    return {};
  }
}

export function storedCredential(host: string): HostCredential | undefined {
  return readHosts()[normalizeHost(host)];
}

export function storeCredential(host: string, cred: HostCredential): void {
  const path = hostsPath();
  mkdirSync(dirname(path), { recursive: true, mode: 0o700 });
  const hosts = readHosts();
  hosts[normalizeHost(host)] = cred;
  writeFileSync(path, `${JSON.stringify(hosts, null, 2)}\n`, { mode: 0o600 });
  // writeFileSync's mode only applies on create — clamp an existing file too.
  chmodSync(path, 0o600);
}

export function deleteCredential(host: string): void {
  const hosts = readHosts();
  if (!(normalizeHost(host) in hosts)) return;
  delete hosts[normalizeHost(host)];
  writeFileSync(hostsPath(), `${JSON.stringify(hosts, null, 2)}\n`, { mode: 0o600 });
}

/** The key for a request against `host`, or undefined when logged out. */
export function resolveApiKey(host: string): string | undefined {
  return process.env["ENGRAMS_API_KEY"] || storedCredential(host)?.apiKey;
}
