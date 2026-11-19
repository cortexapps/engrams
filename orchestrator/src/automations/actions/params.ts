/** Readers of a built-in action's rendered params. */

import { IntegrationActionError } from "./errors.ts";

export function requireString(params: Record<string, unknown>, key: string): string {
  const value = params[key];
  if (typeof value !== "string" || value === "") {
    throw new IntegrationActionError(`missing required string "${key}"`, true);
  }
  return value;
}

export function requireInteger(params: Record<string, unknown>, key: string): number {
  const value = params[key];
  if (typeof value !== "number" || !Number.isInteger(value)) {
    throw new IntegrationActionError(`missing required integer "${key}"`, true);
  }
  return value;
}

export function optionalString(params: Record<string, unknown>, key: string): string | undefined {
  const value = params[key];
  return typeof value === "string" && value !== "" ? value : undefined;
}
