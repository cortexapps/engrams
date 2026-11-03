import { expect, spyOn, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { assertOriginalArguments } from "../mcp/journal-review.ts";
import { canonicalDigest } from "../mcp/hash.ts";
import { MCP_LIMITS } from "../mcp/limits.ts";

const accepted = [
  "0",
  "-0",
  "0.0e999999999999999999999",
  "1e3",
  "1000.00",
  "1.2300e2",
  "0.1",
  "1e-7",
  "9007199254740991",
  "-9007199254740991",
  "5e-324",
  "1e+0003",
  "1e-0007",
];
const rejected = [
  "9007199254740992",
  "9007199254740993",
  "-9007199254740992",
  "-9007199254740993",
  "9.007199254740993e15",
  "0.10000000000000001",
  "1.0000000000000001",
  "9007199254740991.1",
  "1e400",
  "-1e400",
  "1e-400",
  "-1e-400",
  "4e-324",
  "1e20",
];
for (const token of accepted)
  test(`original arguments preserve decimal value ${token}`, () => {
    const raw = `{"x":${token}}`;
    expect(() => assertOriginalArguments(raw, canonicalDigest(JSON.parse(raw)))).not.toThrow();
  });
for (const token of rejected)
  test(`original arguments reject changed decimal value ${token}`, () => {
    const raw = `{"x":${token}}`;
    const parsed = JSON.parse(raw);
    const hash = Number.isFinite(parsed.x) ? canonicalDigest(parsed) : canonicalDigest({ x: null });
    expect(() => assertOriginalArguments(raw, hash)).toThrow("invalid argument JSON");
  });

test("numeric checks cover nested arrays and objects, with numeric strings unchanged", () => {
  const safe = '{"items":[{"x":0.1},[5e-324]],"id":"9007199254740993"}';
  expect(() => assertOriginalArguments(safe, canonicalDigest(JSON.parse(safe)))).not.toThrow();
  const unsafe = '{"items":[{"x":9007199254740993}]}';
  expect(() => assertOriginalArguments(unsafe, canonicalDigest(JSON.parse(unsafe)))).toThrow();
});

test("bounded numeric tokens handle zero, exponent leading zeros and underflow", () => {
  for (const raw of [
    `{"x":0.${"0".repeat(MCP_LIMITS.argumentBytes - 40)}e999999999999999999999}`,
    `{"x":1e${"0".repeat(MCP_LIMITS.argumentBytes - 20)}3}`,
    `{"x":1${"0".repeat(MCP_LIMITS.argumentBytes - 30)}e-${MCP_LIMITS.argumentBytes - 29}}`,
  ]) {
    expect(Buffer.byteLength(raw)).toBeLessThanOrEqual(MCP_LIMITS.argumentBytes);
    expect(() => assertOriginalArguments(raw, canonicalDigest(JSON.parse(raw)))).not.toThrow();
  }
  for (const raw of [
    `{"x":0.${"0".repeat(MCP_LIMITS.argumentBytes - 20)}1}`,
    `{"x":1e${"9".repeat(MCP_LIMITS.argumentBytes - 20)}}`,
  ]) {
    expect(Buffer.byteLength(raw)).toBeLessThanOrEqual(MCP_LIMITS.argumentBytes);
    expect(() => assertOriginalArguments(raw, canonicalDigest(JSON.parse(raw)))).toThrow();
  }
});

test("runtime supplies the original numeric token to the reviver", () => {
  let source: string | undefined;
  JSON.parse("9007199254740993", (_key, value, context?: { source?: string }) => {
    source = context?.source;
    return value;
  });
  expect(source).toBe("9007199254740993");
});

test("missing numeric source fails closed", () => {
  const parse = JSON.parse;
  const hash = canonicalDigest({ x: 1 });
  const patched = spyOn(JSON, "parse").mockImplementation((raw, reviver) =>
    parse(
      raw,
      reviver &&
        function (key, value) {
          return reviver.call(this, key, value);
        },
    ),
  );
  try {
    expect(() => assertOriginalArguments('{"x":1}', hash)).toThrow("invalid argument JSON");
  } finally {
    patched.mockRestore();
  }
});

test("near-budget internal and trailing zero runs finish in a bounded child", () => {
  const parser = new URL("../mcp/arguments.ts", import.meta.url).href;
  const child = spawnSync(
    process.execPath,
    [
      "--no-env-file",
      "--no-install",
      "--eval",
      `
    import { parseArgumentJson } from ${JSON.stringify(parser)};
    const limit = ${MCP_LIMITS.argumentBytes};
    const accepted = '{"x":0.1' + "0".repeat(limit - 9) + '}';
    const rejected = '{"x":0.1' + "0".repeat(limit - 10) + '1}';
    if (Buffer.byteLength(accepted) !== limit || Buffer.byteLength(rejected) !== limit)
      throw Error("fixture byte count changed");
    if (JSON.stringify(parseArgumentJson(accepted)) !== '{"x":0.1}')
      throw Error("trailing-zero value changed");
    let denied = false;
    try { parseArgumentJson(rejected); }
    catch (error) { denied = error instanceof Error && error.message === "numeric argument loses precision"; }
    if (!denied) throw Error("internal-zero precision loss accepted");
    console.log("checked two full-limit numeric tokens");
  `,
    ],
    { encoding: "utf8", timeout: 10000, killSignal: "SIGKILL" },
  );
  expect(child.error).toBeUndefined();
  expect(child.status).toBe(0);
  expect(child.stderr).toBe("");
  expect(child.stdout.trim()).toBe("checked two full-limit numeric tokens");
}, 15000);
