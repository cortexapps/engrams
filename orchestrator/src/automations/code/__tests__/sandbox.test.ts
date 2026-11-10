import { describe, expect, test } from "bun:test";

import { CODE_LOG_MAX_ENTRIES, CODE_OUTPUT_MAX_BYTES, evaluateCode } from "../sandbox.ts";

/** The permanent Bun-compat gate for the QuickJS-on-WASM sandbox (ADR 0119
 * D6). If a Bun or variant upgrade breaks instantiation, limits, or dispose
 * discipline, this suite is where it surfaces.
 */

describe("evaluateCode — values and contract", () => {
  test("value round-trip: objects, arrays, unicode, null", async () => {
    const result = await evaluateCode(
      `export default ({ event, inputs }) => ({
         title: event.title + " ✓",
         labels: [...event.labels, "automation"],
         n: inputs.n * 2,
         nothing: null,
       });`,
      { event: { title: "héllo — 世界", labels: ["a"] }, inputs: { n: 21 } },
      "value",
    );
    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(result.value).toEqual({
        title: "héllo — 世界 ✓",
        labels: ["a", "automation"],
        n: 42,
        nothing: null,
      });
      expect(result.durationMs).toBeGreaterThanOrEqual(0);
    }
  });

  test("boolean mode: strict true/false, everything else is a ContractError", async () => {
    const yes = await evaluateCode(`export default () => true;`, {}, "boolean");
    expect(yes.ok).toBe(true);
    const no = await evaluateCode(`export default () => false;`, {}, "boolean");
    expect(no.ok).toBe(true);
    if (no.ok) expect(no.value).toBe(false);
    const truthy = await evaluateCode(`export default () => 1;`, {}, "boolean");
    expect(truthy.ok).toBe(false);
    if (!truthy.ok) expect(truthy.error.name).toBe("ContractError");
  });

  test("missing default export / undefined return are ContractErrors", async () => {
    const noDefault = await evaluateCode(`export const x = 1;`, {}, "value");
    expect(noDefault.ok).toBe(false);
    if (!noDefault.ok) expect(noDefault.error.name).toBe("ContractError");

    const undef = await evaluateCode(`export default () => undefined;`, {}, "value");
    expect(undef.ok).toBe(false);
    if (!undef.ok) expect(undef.error.message).toContain("undefined");
  });

  test("async default exports are a clear ContractError, never a silent {}", async () => {
    // JSON.stringify(<Promise>) is "{}"; without the thenable guard an async
    // export would succeed with a silently wrong empty object.
    const asyncValue = await evaluateCode(
      `export default async ({ n }) => ({ total: n * 2 });`,
      { inputs: { n: 2 } },
      "value",
    );
    expect(asyncValue.ok).toBe(false);
    if (!asyncValue.ok) {
      expect(asyncValue.error.name).toBe("ContractError");
      expect(asyncValue.error.message).toContain("async code is not supported");
    }

    const asyncBool = await evaluateCode(`export default async () => true;`, {}, "boolean");
    expect(asyncBool.ok).toBe(false);
    if (!asyncBool.ok) expect(asyncBool.error.message).toContain("async code is not supported");

    // A hand-rolled thenable is caught the same way.
    const thenable = await evaluateCode(
      `export default () => ({ then: (resolve) => resolve(1) });`,
      {},
      "value",
    );
    expect(thenable.ok).toBe(false);
    if (!thenable.ok) expect(thenable.error.name).toBe("ContractError");
  });

  test("syntax errors surface with name and message", async () => {
    const result = await evaluateCode(`export default ({) => 1;`, {}, "value");
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.error.name).toBe("SyntaxError");
  });

  test("thrown errors carry the automation.js line number", async () => {
    const result = await evaluateCode(
      `export default () => {\n\n  throw new Error("boom");\n};`,
      {},
      "value",
    );
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error.name).toBe("Error");
      expect(result.error.message).toBe("boom");
      expect(result.error.line).toBe(3);
    }
  });
});

describe("evaluateCode — limits", () => {
  test("infinite loop interrupts as TimeoutError within ~1s wall", async () => {
    const t0 = performance.now();
    const result = await evaluateCode(`export default () => { while (true) {} };`, {}, "value");
    const wall = performance.now() - t0;
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.error.name).toBe("TimeoutError");
    expect(wall).toBeLessThan(1000);
  });

  test("memory bomb dies as MemoryError and the process survives", async () => {
    const result = await evaluateCode(
      `export default () => { const a = []; for (;;) a.push(new Array(1e6).fill(0)); };`,
      {},
      "value",
    );
    expect(result.ok).toBe(false);
    if (!result.ok) expect(["MemoryError", "TimeoutError"]).toContain(result.error.name);
    // Still healthy:
    const after = await evaluateCode(`export default () => 42;`, {}, "value");
    expect(after.ok).toBe(true);
  });

  test("deep recursion dies as a stack error, not a process crash", async () => {
    const result = await evaluateCode(
      `export default () => { const f = () => f(); return f(); };`,
      {},
      "value",
    );
    expect(result.ok).toBe(false);
    if (!result.ok)
      expect(["StackError", "InternalError", "RangeError"]).toContain(result.error.name);
  });

  test("oversized output is an OutputError", async () => {
    const result = await evaluateCode(
      `export default () => "x".repeat(${CODE_OUTPUT_MAX_BYTES});`,
      {},
      "value",
    );
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.error.name).toBe("OutputError");
  });
});

describe("evaluateCode — the cage", () => {
  test("no fetch, timers, require, or process in the guest", async () => {
    const result = await evaluateCode(
      `export default () => [typeof fetch, typeof setTimeout, typeof setInterval, typeof require, typeof process, typeof XMLHttpRequest];`,
      {},
      "value",
    );
    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(result.value).toEqual([
        "undefined",
        "undefined",
        "undefined",
        "undefined",
        "undefined",
        "undefined",
      ]);
    }
  });

  test("input is frozen in the guest and the host object is unaffected", async () => {
    const hostEvent = { title: "original", nested: { n: 1 } };
    const result = await evaluateCode(
      `export default ({ event }) => {
         let threw = false;
         try { event.title = "mutated"; } catch { threw = true; }
         try { event.nested.n = 99; } catch {}
         return { threw, title: event.title, n: event.nested.n };
       };`,
      { event: hostEvent },
      "value",
    );
    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(result.value).toMatchObject({ title: "original", n: 1 });
    }
    expect(hostEvent.title).toBe("original");
    expect(hostEvent.nested.n).toBe(1);
  });

  test("console capture with entry and byte caps", async () => {
    const result = await evaluateCode(
      `export default () => {
         console.log("hello", { a: 1 });
         console.warn("careful");
         for (let i = 0; i < 500; i += 1) console.log("spam", i);
         return 1;
       };`,
      {},
      "value",
    );
    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(result.logs[0]).toBe('log: hello {"a":1}');
      expect(result.logs[1]).toBe("warn: careful");
      expect(result.logs.length).toBeLessThanOrEqual(CODE_LOG_MAX_ENTRIES);
    }
  });

  test("logs survive a guest error", async () => {
    const result = await evaluateCode(
      `export default () => { console.log("before the boom"); throw new Error("boom"); };`,
      {},
      "value",
    );
    expect(result.ok).toBe(false);
    expect(result.logs).toEqual(["log: before the boom"]);
  });
});

describe("evaluateCode — dispose discipline", () => {
  test("host WebAssembly stack exhaustion leaves the next evaluation healthy", async () => {
    const programs = [
      {
        source: `export default () => { globalThis.previousEvaluation = true; const f = () => f(); return f(); };`,
        outcome: "stack",
      },
      {
        source: `export default () => { const value = { [Symbol.toPrimitive]() { return +value; } }; return +value; };`,
        outcome: "stack",
      },
      {
        source: `export default () => ${"(".repeat(512)}1${")".repeat(512)};`,
        outcome: "finite-or-stack",
      },
      {
        source: `export default () => {
          const stringify = JSON.stringify;
          let calls = 0;
          JSON.stringify = (value) => ++calls === 1 ? stringify(value) : {
            toString() { return this.toString(); },
          };
          return 42;
        };`,
        outcome: "log-failure",
      },
      {
        source: `export default () => {
          let calls = 0;
          JSON.stringify = () => ++calls === 1 ? "42" : "not JSON";
          return 42;
        };`,
        outcome: "host-failure",
      },
      {
        source: `export default () => { throw Promise.resolve(42); };`,
        outcome: "guest-promise",
      },
    ];
    const source = `
      import assert from "node:assert/strict";
      import { evaluateCode } from ${JSON.stringify(new URL("../sandbox.ts", import.meta.url).href)};
      const { QuickJSContext, QuickJSRuntime } = await import(${JSON.stringify(import.meta.resolve("quickjs-emscripten-core"))});
      const contextDispose = QuickJSContext.prototype.dispose;
      const runtimeDispose = QuickJSRuntime.prototype.dispose;
      let contexts = 0, runtimes = 0;
      QuickJSContext.prototype.dispose = function () { contexts++; return contextDispose.call(this); };
      QuickJSRuntime.prototype.dispose = function () { runtimes++; return runtimeDispose.call(this); };
      try { for (const program of ${JSON.stringify(programs)}) {
        const before = { contexts, runtimes };
        const result = await evaluateCode(program.source, {}, "value");
        if (program.outcome === "finite-or-stack" && result.ok) {
          assert.equal(result.value, 1);
        } else {
          assert.equal(result.ok, false);
          if (program.outcome === "guest-promise") {
            assert.equal(contexts - before.contexts, 1);
            assert.equal(runtimes - before.runtimes, 1);
          } else if (program.outcome === "host-failure") {
            assert.equal(result.error.name, "SandboxError");
          } else if (program.outcome !== "log-failure") {
            assert.ok(["StackError", "InternalError", "RangeError"].includes(result.error.name),
              JSON.stringify(result.error));
          }
        }
        const after = await evaluateCode(
          'export default () => ({ value: 42, inherited: typeof globalThis.previousEvaluation });', {}, "value");
        assert.equal(after.ok, true);
        assert.deepEqual(after.value, { value: 42, inherited: "undefined" });
      } } finally {
        QuickJSContext.prototype.dispose = contextDispose;
        QuickJSRuntime.prototype.dispose = runtimeDispose;
      }
    `;
    const child = Bun.spawn([process.execPath, "--no-env-file", "--eval", source], {
      env: {
        NODE_ENV: "test",
        BUN_JSC_useWasmIPInt: "false",
        BUN_JSC_useBBQJIT: "true",
        BUN_JSC_useOMGJIT: "false",
      },
      stdout: "pipe",
      stderr: "pipe",
      timeout: 10_000,
    });
    try {
      const [exitCode, stdout, stderr] = await Promise.all([
        child.exited,
        new Response(child.stdout).text(),
        new Response(child.stderr).text(),
      ]);
      expect(exitCode, stderr).toBe(0);
      expect(stdout).toBe("");
      expect(stderr).toBe("");
    } finally {
      if (child.exitCode === null) child.kill();
      await child.exited;
    }
  }, 20_000);

  test("200 sequential evaluations stay healthy", async () => {
    for (let i = 0; i < 200; i += 1) {
      const result = await evaluateCode(
        `export default ({ inputs }) => inputs.i + 1;`,
        { inputs: { i } },
        "value",
      );
      expect(result.ok).toBe(true);
      if (result.ok) expect(result.value).toBe(i + 1);
    }
  }, 30_000);
});
