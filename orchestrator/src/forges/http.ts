/** Readers of an IntegrationOp response that every forge client uses. */

import type { IntegrationOpResult } from "../integrations/run-op.ts";
import { ForgeRequestError } from "./forge.ts";

export type Json = Record<string, unknown>;

export function isObject(value: unknown): value is Json {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function decodeBody(body: Uint8Array): string {
  return new TextDecoder().decode(body);
}

export function succeeded(result: IntegrationOpResult): boolean {
  return result.status >= 200 && result.status < 300;
}

/** A response with a status that is not a success. */
export function responseError(forge: string, operation: string, result: IntegrationOpResult): ForgeRequestError {
  const detail = decodeBody(result.body).trim();
  return new ForgeRequestError(
    result.status,
    `${operation} failed with ${forge} status ${result.status}${detail ? `: ${detail}` : ""}`,
    detail,
  );
}

/** A 2xx response whose body is not what the operation returns. It carries
 * a 2xx status, so no caller treats it as permanent. */
export function unusable(operation: string, what: string): ForgeRequestError {
  return new ForgeRequestError(200, `${operation} returned ${what}`, "");
}

export function parseJson(result: IntegrationOpResult, operation: string): unknown {
  try {
    return JSON.parse(decodeBody(result.body));
  } catch {
    throw unusable(operation, "invalid JSON");
  }
}
