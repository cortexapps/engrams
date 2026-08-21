import { describe, expect, test } from "bun:test";

import {
  CODE_LOG_MAX_ENTRIES,
  CODE_OUTPUT_MAX_BYTES,
  evaluateCode,
} from "../sandbox.ts";

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
    if (!result.ok) expect(["StackError", "InternalError", "RangeError"]).toContain(result.error.name);
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
  test("200 sequential evaluations stay healthy", async () => {
    for (let i = 0; i < 200; i += 1) {
      const result = await evaluateCode(`export default ({ inputs }) => inputs.i + 1;`, { inputs: { i } }, "value");
      expect(result.ok).toBe(true);
      if (result.ok) expect(result.value).toBe(i + 1);
    }
  }, 30_000);
});
