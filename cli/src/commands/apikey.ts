/**
 * engrams apikey … — admin management of global service-account keys
 * (ADR 0086). Your own CLI login keys are managed by `auth login/logout`;
 * this is the CI/service-key surface (mirrors /settings/api-keys).
 */

import type { Clients } from "../client.ts";
import { fail, failWith, printJson, table, truncate } from "../output.ts";
import type { ApiKeyMeta } from "../gen/engram/app/v1/api_key_pb.ts";

function keyJson(k: ApiKeyMeta) {
  return {
    id: k.id,
    name: k.name,
    role: k.role,
    start: k.start,
    owner_email: k.ownerEmail,
    created_at: k.createdAt,
    expires_at: k.expiresAt,
    last_used_at: k.lastUsedAt,
  };
}

export async function create(
  c: Clients,
  name: string,
  role: string,
  expiresAt: string,
  json: boolean,
): Promise<void> {
  if (role !== "admin" && role !== "user") fail('--role must be "admin" or "user"');
  const resp = await c.apiKey.createApiKey({ name, role, expiresAt }).catch(failWith);
  if (json) {
    printJson({ ...keyJson(resp.meta!), key: resp.key });
    return;
  }
  // The one-time plaintext goes to stdout ALONE so `$(engrams apikey create …)`
  // captures exactly the key; everything human goes to stderr.
  console.error(`created key "${name}" (role=${role}, id=${resp.meta?.id})`);
  console.error("this is the only time the key is shown — store it now:");
  console.log(resp.key);
}

export async function list(c: Clients, json: boolean): Promise<void> {
  const resp = await c.apiKey.listApiKeys({}).catch(failWith);
  if (json) {
    printJson({ keys: resp.keys.map(keyJson) });
    return;
  }
  if (resp.keys.length === 0) {
    console.log("(no API keys)");
    return;
  }
  table(
    ["ID", "NAME", "ROLE", "KEY", "OWNER"],
    resp.keys.map((k) => [
      k.id,
      truncate(k.name, 24),
      k.role,
      `${k.start}…`,
      truncate(k.ownerEmail, 32),
    ]),
    [36, 24, 6, 14, 32],
  );
}

export async function revoke(c: Clients, id: string, json: boolean): Promise<void> {
  const resp = await c.apiKey.revokeApiKey({ id }).catch(failWith);
  if (json) printJson({ revoked: resp.revoked });
  else console.log(resp.revoked ? "revoked" : "no such key (already revoked?)");
}
