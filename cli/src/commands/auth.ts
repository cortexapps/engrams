/**
 * engrams auth login|logout|status — the `gh auth` model.
 *
 * login: RFC 8628 device flow against the orchestrator's better-auth
 * device-authorization plugin, then ONE exchange RPC:
 *   1. POST /api/auth/device/code {client_id} → user_code + verification_uri
 *   2. print the code, open the browser to the /device approval page
 *   3. poll POST /api/auth/device/token until approved → short-lived session
 *   4. ApiKeyService.CreateCliKey (authed by that session as a Bearer) →
 *      durable user-owned engk_… key → hosts.json (0600); session discarded.
 *
 * logout: RevokeCliKey (authed by the stored key itself) + drop the entry.
 * status: GET /api/auth/get-session with the resolved key → who am I.
 */

import { spawn } from "node:child_process";
import { hostname } from "node:os";
import { createInterface } from "node:readline/promises";

import { authHeaders, clientsWith } from "../client.ts";
import {
  DEVICE_CLIENT_ID,
  deleteCredential,
  hostsPath,
  resolveApiKey,
  storeCredential,
  storedCredential,
} from "../config.ts";
import { fail, failWith, printJson } from "../output.ts";

interface DeviceGrant {
  device_code: string;
  user_code: string;
  verification_uri: string;
  verification_uri_complete: string;
  expires_in: number;
  interval: number;
}

/** Fire-and-forget the platform browser opener; the URL is always printed
 *  too, so a headless box (SSH, container) just falls back to copy/paste. */
function openBrowser(url: string): void {
  const cmd =
    process.platform === "darwin" ? "open" : process.platform === "win32" ? "start" : "xdg-open";
  try {
    spawn(cmd, [url], { stdio: "ignore", detached: true }).on("error", () => {}).unref();
  } catch {
    // Printed URL is the fallback.
  }
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

export async function login(host: string): Promise<void> {
  if (process.env["ENGRAMS_API_KEY"]) {
    fail(
      "ENGRAMS_API_KEY is set — it would shadow any stored login. Unset it to use `auth login`.",
    );
  }

  // 1. Request the device grant.
  const codeRes = await fetch(`${host}/api/auth/device/code`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ client_id: DEVICE_CLIENT_ID }),
  }).catch((e: unknown) => fail(`cannot reach ${host}: ${e instanceof Error ? e.message : e}`));
  if (!codeRes.ok) {
    fail(`device authorization rejected (HTTP ${codeRes.status}) — is this an engrams host?`);
  }
  const grant = (await codeRes.json()) as DeviceGrant;

  // 2. Hand the human their code (gh's exact choreography).
  const url = grant.verification_uri_complete || grant.verification_uri;
  console.error(`First, copy your one-time code: ${grant.user_code}`);
  console.error(`Then approve the CLI in your browser: ${url}`);
  const stdin = createInterface({ input: process.stdin, output: process.stderr });
  await stdin.question("Press Enter to open the browser…");
  stdin.close();
  openBrowser(url);

  // 3. Poll for the approval.
  const deadline = Date.now() + grant.expires_in * 1000;
  let intervalMs = Math.max(1, grant.interval) * 1000;
  let accessToken: string | undefined;
  while (Date.now() < deadline) {
    await sleep(intervalMs);
    const res = await fetch(`${host}/api/auth/device/token`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        grant_type: "urn:ietf:params:oauth:grant-type:device_code",
        device_code: grant.device_code,
        client_id: DEVICE_CLIENT_ID,
      }),
    });
    if (res.ok) {
      accessToken = ((await res.json()) as { access_token: string }).access_token;
      break;
    }
    const err = ((await res.json().catch(() => ({}))) as { error?: string }).error;
    if (err === "authorization_pending") continue;
    if (err === "slow_down") {
      intervalMs += 5000;
      continue;
    }
    if (err === "access_denied") fail("the request was denied in the browser");
    if (err === "expired_token") break;
    fail(`device token poll failed: ${err ?? `HTTP ${res.status}`}`);
  }
  if (!accessToken) fail("the one-time code expired — run `engrams auth login` again");

  // 4. Exchange the short-lived session for the durable user-owned key.
  const exchange = clientsWith(host, { bearer: accessToken });
  const minted = await exchange.apiKey
    .createCliKey({ name: `cli:${hostname()}` })
    .catch(failWith);

  // Who did we just become? (Also sanity-proves the minted key works.)
  const me = await whoami(host, minted.key);
  storeCredential(host, {
    apiKey: minted.key,
    keyId: minted.meta?.id ?? "",
    ...(me?.email ? { user: me.email } : {}),
  });
  console.error(`Logged in to ${host} as ${me?.email ?? "(unknown)"}`);
  console.error(`Credential saved to ${hostsPath()}`);
}

interface Me {
  email?: string;
  name?: string;
  role?: string;
}

/** Resolve the identity a key acts as via better-auth's get-session. */
async function whoami(host: string, apiKey: string): Promise<Me | null> {
  const res = await fetch(`${host}/api/auth/get-session`, {
    headers: authHeaders({ apiKey }),
  });
  if (!res.ok) return null;
  const body = (await res.json()) as {
    user?: { email?: string; name?: string; role?: string };
  } | null;
  return body?.user ?? null;
}

export async function logout(host: string): Promise<void> {
  const stored = storedCredential(host);
  if (!stored) fail(`no stored login for ${host}`);
  // Best-effort server-side revoke, authenticated by the key being revoked.
  // A dead/already-revoked key must not wedge logout — drop the entry anyway.
  try {
    if (stored.keyId) {
      await clientsWith(host, { apiKey: stored.apiKey }).apiKey.revokeCliKey({
        id: stored.keyId,
      });
    }
  } catch (e) {
    console.error(
      `engrams: warning: server-side revoke failed (${e instanceof Error ? e.message : e}); ` +
        "removing the local credential anyway — revoke it in Settings → API keys",
    );
  }
  deleteCredential(host);
  console.error(`Logged out of ${host}`);
}

export async function status(host: string, json: boolean): Promise<void> {
  const key = resolveApiKey(host);
  const source = process.env["ENGRAMS_API_KEY"] ? "ENGRAMS_API_KEY" : hostsPath();
  if (!key) {
    if (json) printJson({ host, loggedIn: false });
    else console.log(`${host}: not logged in — run \`engrams auth login\``);
    process.exitCode = 1;
    return;
  }
  const me = await whoami(host, key);
  if (json) {
    printJson({ host, loggedIn: me !== null, user: me?.email, role: me?.role, source });
    if (me === null) process.exitCode = 1;
    return;
  }
  if (me === null) {
    console.log(`${host}: stored credential is invalid or revoked (from ${source})`);
    process.exitCode = 1;
    return;
  }
  console.log(`${host}: logged in as ${me.email} (${me.role ?? "user"}) via ${source}`);
}
