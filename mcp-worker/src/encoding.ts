import { createHash } from "node:crypto";
export function object(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
export function canonical(value: unknown): string {
  if (value === null || typeof value === "string" || typeof value === "boolean")
    return JSON.stringify(value);
  if (typeof value === "number" && Number.isFinite(value))
    return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (object(value))
    return `{${Object.keys(value)
      .sort()
      .filter((k) => value[k] !== undefined)
      .map((k) => `${JSON.stringify(k)}:${canonical(value[k])}`)
      .join(",")}}`;
  throw Error("invalid_json");
}
export function digest(value: unknown) {
  return createHash("sha256").update(canonical(value)).digest("hex");
}
export function bytes(value: unknown) {
  return Buffer.byteLength(JSON.stringify(value));
}
export function redact(text: string, secrets: string[]): string {
  for (const secret of secrets)
    if (secret) text = text.split(secret).join("[redacted]");
  return text;
}
