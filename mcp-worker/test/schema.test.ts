import assert from "node:assert/strict";
import { test } from "node:test";
import { validateSchemas } from "../src/schema.ts";
test("independent dialects, roots and local references retain their meaning", async () => {
  const a = {
    $id: "https://schema.example/a",
    $schema: "http://json-schema.org/draft-07/schema#",
    definitions: { value: { type: "integer", minimum: 1 } },
    properties: { value: { $ref: "#/definitions/value" } },
  };
  const b = {
    $id: "https://schema.example/a",
    $schema: "https://json-schema.org/draft/2020-12/schema",
    $defs: { value: { type: "integer", maximum: 3 } },
    properties: { value: { $ref: "#/$defs/value" } },
  };
  assert.equal(await validateSchemas([a, b], { value: 2 }), true);
  assert.equal(await validateSchemas([a, b], { value: 0 }), false);
  assert.equal(await validateSchemas([a, b], { value: 4 }), false);
  assert.equal(await validateSchemas([{ type: "number" }], "2"), false);
});
test("unsupported dialects, remote references and executable formats fail closed", async () => {
  for (const schema of [
    { $schema: "https://example.com/schema" },
    { $ref: "https://example.com/schema" },
    { format: "executable" },
    { type: "string", pattern: "[" },
  ])
    await assert.rejects(validateSchemas([schema]));
});
test("hostile regular expression is killed in disposable process", async () => {
  const begin = performance.now();
  await assert.rejects(
    validateSchemas(
      [{ type: "string", pattern: "^(a+)+$" }],
      "a".repeat(100) + "!",
    ),
    { message: "schema_deadline" },
  );
  assert.ok(performance.now() - begin < 6500);
  assert.equal(await validateSchemas([{ type: "integer" }], 1), true);
});
test("standard formats validate in the child and ordinary data keys remain data", async () => {
  assert.equal(
    await validateSchemas(
      [{ type: "string", format: "date-time" }],
      "2026-11-02T12:00:00Z",
    ),
    true,
  );
  assert.equal(
    await validateSchemas([{ type: "string", format: "date-time" }], "invalid"),
    false,
  );
  assert.equal(
    await validateSchemas(
      [
        {
          properties: {
            $ref: { const: "https://example.com/a" },
            $schema: { const: "data" },
          },
        },
      ],
      { $ref: "https://example.com/a", $schema: "data" },
    ),
    true,
  );
});
test("schema processes have a shared cap and cancellation kills the child", async () => {
  const controllers = Array.from({ length: 4 }, () => new AbortController());
  const pending = controllers.map((c) =>
    validateSchemas(
      [{ type: "string", pattern: "^(a+)+$" }],
      "a".repeat(100) + "!",
      c.signal,
    ),
  );
  await assert.rejects(validateSchemas([{}]), /capacity/);
  for (const controller of controllers) controller.abort();
  for (const result of await Promise.allSettled(pending))
    assert.equal(result.status, "rejected");
});
test("trusted bootstrap delay cannot extend the schema timer and can be canceled", async (t) => {
  const { default: childProcess } = await import("node:child_process");
  const { pathToFileURL } = await import("node:url");
  const original = childProcess.spawn;
  let firstWrite = 0;
  const mock = t.mock.method(
    childProcess,
    "spawn",
    (
      command: string,
      args: readonly string[],
      options: import("node:child_process").SpawnOptionsWithoutStdio,
    ) => {
      const path = args.at(-1)!;
      const code = `await new Promise(r=>setTimeout(r,600)); await import(${JSON.stringify(pathToFileURL(path).href)});`;
      const child = original(
        command,
        [...args.slice(0, -1), "--input-type=module", "--eval", code],
        options,
      );
      child.stdin?.once("finish", () => {
        firstWrite = performance.now();
      });
      return child;
    },
  );
  await assert.rejects(
    validateSchemas(
      [{ type: "string", pattern: "^(a+)+$" }],
      "a".repeat(100) + "!",
    ),
    /schema_deadline/,
  );
  assert.ok(firstWrite > 0);
  assert.ok(performance.now() - firstWrite < 2500);
  const controller = new AbortController();
  const pending = validateSchemas([{}], {}, controller.signal);
  controller.abort();
  await assert.rejects(pending, /schema_aborted/);
  mock.mock.restore();
});
test("the actual disposable V8 heap ceiling is at most 128 MiB", async (t) => {
  const { default: childProcess } = await import("node:child_process");
  const { pathToFileURL } = await import("node:url");
  const original = childProcess.spawn;
  t.mock.method(
    childProcess,
    "spawn",
    (
      command: string,
      args: readonly string[],
      options: import("node:child_process").SpawnOptionsWithoutStdio,
    ) => {
      const code = `import{getHeapStatistics}from'node:v8';if(getHeapStatistics().heap_size_limit>128*1024*1024)process.exit(9);await import(${JSON.stringify(pathToFileURL(args.at(-1)!).href)});`;
      return original(
        command,
        [...args.slice(0, -1), "--input-type=module", "--eval", code],
        options,
      );
    },
  );
  assert.equal(await validateSchemas([{ type: "integer" }], 1), true);
});

test("split UTF-8 preserves schema constants and argument values", async () => {
  const { spawn } = await import("node:child_process");
  const results: string[] = [];
  for (const [value, splitValue] of [
    ["é", false],
    ["��", false],
    ["é", true],
  ] as const) {
    const child = spawn(
      process.execPath,
      [
        "--max-old-space-size=125",
        "--max-semi-space-size=1",
        new URL("../src/schema-child.ts", import.meta.url).pathname,
      ],
      {
        env: {},
        stdio: ["pipe", "pipe", "ignore"],
        signal: AbortSignal.timeout(5000),
      },
    );
    const result = new Promise<string>((resolve, reject) => {
      let output = "";
      let sent = false;
      child.on("error", reject);
      child.stdout.on("data", (chunk: Buffer) => {
        output += chunk.toString();
        if (!sent && output.startsWith("ready\n")) {
          sent = true;
          const payload = Buffer.from(
            JSON.stringify({ documents: [{ const: "é" }], value }),
          );
          const split =
            (splitValue
              ? payload.lastIndexOf(Buffer.from("é"))
              : payload.indexOf(Buffer.from("é"))) + 1;
          child.stdin.write(payload.subarray(0, split));
          setTimeout(() => child.stdin.end(payload.subarray(split)), 50);
        }
      });
      child.on("close", (code) => {
        if (code !== 0) reject(Error("child_failed"));
        else resolve(output);
      });
    });
    results.push(await result);
  }
  assert.deepEqual(results, ["ready\ntrue", "ready\nfalse", "ready\ntrue"]);
});
