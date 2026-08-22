/** Deterministic client ids for client_id-idempotent actions (ADR 0119 D5).
 *
 * RFC 4122 name-based UUID (version 5, SHA-1) over `<runId>:<stepPath>` in a
 * fixed engrams namespace: the same block in the same run always mints the
 * same id, so a replayed or retried create adopts instead of duplicating.
 */

import { createHash } from "node:crypto";

/** Fixed namespace (a randomly generated v4, constant forever). */
const NAMESPACE = "9c3f1f0a-4a5d-4d0b-9e6f-2f0f4b6a7c11";

function uuidBytes(uuid: string): Uint8Array {
  const hex = uuid.replaceAll("-", "");
  const bytes = new Uint8Array(16);
  for (let i = 0; i < 16; i += 1) {
    bytes[i] = Number.parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  }
  return bytes;
}

export function actionClientId(runId: string, stepPath: string): string {
  const hash = createHash("sha1");
  hash.update(uuidBytes(NAMESPACE));
  hash.update(`${runId}:${stepPath}`);
  const digest = hash.digest().subarray(0, 16);
  digest[6] = (digest[6]! & 0x0f) | 0x50; // version 5
  digest[8] = (digest[8]! & 0x3f) | 0x80; // RFC 4122 variant
  const hex = [...digest].map((b) => b.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
