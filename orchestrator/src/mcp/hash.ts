import { createHash } from "node:crypto";

/** Strict JSON canonicalization: no silent removal of unsupported values. */
export function canonicalJson(value: unknown): string {
  const seen = new Set<object>();
  function visit(v: unknown): unknown {
    if (v === null || typeof v === "string" || typeof v === "boolean") return v;
    if (typeof v === "number" && Number.isFinite(v)) return v;
    if (typeof v !== "object" || v === null || seen.has(v))
      throw new Error("expected finite, acyclic JSON");
    seen.add(v);
    let result: unknown;
    if (Array.isArray(v)) result = v.map(visit);
    else {
      if (Object.getPrototypeOf(v) !== Object.prototype && Object.getPrototypeOf(v) !== null)
        throw new Error("expected plain JSON object");
      result = Object.fromEntries(
        Object.keys(v)
          .sort()
          .map((k) => [k, visit((v as Record<string, unknown>)[k])]),
      );
    }
    seen.delete(v);
    return result;
  }
  return JSON.stringify(visit(value));
}

export function canonicalDigest(value: unknown): string {
  return `sha256:${createHash("sha256").update(canonicalJson(value)).digest("hex")}`;
}
