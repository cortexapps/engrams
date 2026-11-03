/** Exact decimal form, with bounded scale arithmetic and no expanded exponent. */
function decimal(token: string, value: number): string {
  const match = /^(-?)(\d+)(?:\.(\d+))?(?:[eE]([+-]?\d+))?$/.exec(token);
  if (!match) throw Error("invalid numeric argument");
  const fraction = match[3] ?? "";
  const digits = (match[2]! + fraction).replace(/^0+/, "");
  // Zero notation and sign are equivalent, including -0 serialized as 0.
  if (!digits) return "0";
  if (value === 0) throw Error("numeric argument underflow");
  const exponent = Number(match[4] ?? "0");
  // A finite nonzero value cannot need a larger exponent after its digits cancel.
  if (!Number.isSafeInteger(exponent) || Math.abs(exponent) > token.length + 400)
    throw Error("numeric argument exponent exceeds limit");
  let end = digits.length;
  while (end > 0 && digits[end - 1] === "0") end--;
  const coefficient = digits.slice(0, end);
  return `${match[1]}${coefficient}e${exponent - fraction.length + digits.length - coefficient.length}`;
}

/** Preserve the decimal value that the worker will serialize, not binary exactness. */
export function parseArgumentJson(raw: string): unknown {
  return JSON.parse(raw, (_key, value: unknown, context?: { source?: string }) => {
    if (typeof value === "number") {
      if (
        typeof context?.source !== "string" ||
        !Number.isFinite(value) ||
        (Number.isInteger(value) && !Number.isSafeInteger(value)) ||
        decimal(context.source, value) !== decimal(JSON.stringify(value), value)
      )
        throw Error("numeric argument loses precision");
    }
    return value;
  });
}
