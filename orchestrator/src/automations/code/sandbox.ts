/** The Code block sandbox: QuickJS compiled to WASM (ADR 0119 D6).
 *
 * User JavaScript runs as an ES module whose default export is called with a
 * frozen JSON copy of the run context. The cage is by construction: a bare
 * QuickJS context has no fetch, no timers, no require, no process, and this
 * module installs no host functions — only a console shim that collects logs.
 *
 * Each evaluation owns a fresh WASM module and capped WebAssembly.Memory.
 * QuickJS's memory accounting does not bound all linear-memory growth, and
 * linear memory does not shrink. Independent instances bound each evaluation
 * and let the host collect a failed instance without re-entering native code.
 */

import type {
  QuickJSContext,
  QuickJSHandle,
  QuickJSRuntime,
  QuickJSWASMModule,
} from "quickjs-emscripten-core";

export const CODE_MEMORY_LIMIT_BYTES = 32 * 1024 * 1024;
export const CODE_CPU_BUDGET_MS = 250;
/** QuickJS's WASM shadow-stack limit; the host has a separate stack limit. */
export const CODE_MAX_STACK_BYTES = 512 * 1024;
export const CODE_OUTPUT_MAX_BYTES = 256 * 1024;
/** Hard ceiling on the per-eval WASM linear memory (backstops QuickJS's own
 * accounting, which big fast-array allocations evade). */
export const WASM_MEMORY_MAX_BYTES = 64 * 1024 * 1024;
export const CODE_LOG_MAX_ENTRIES = 100;
export const CODE_LOG_MAX_BYTES = 16 * 1024;

const WASM_PAGE_BYTES = 64 * 1024;
const WASM_MEMORY_INITIAL_PAGES = 256; // 16 MiB — the module's working floor.

export interface CodeInput {
  event?: unknown;
  steps?: unknown;
  inputs?: unknown;
  trigger?: unknown;
}

export type CodeMode = "value" | "boolean";

export interface CodeErrorShape {
  name: string;
  message: string;
  line?: number;
}

export type CodeOutcome =
  | { ok: true; value: unknown; durationMs: number; logs: string[] }
  | { ok: false; error: CodeErrorShape; durationMs: number; logs: string[] };

const GUEST_PRELUDE = `
(() => {
  const logs = [];
  let logBytes = 0;
  const push = (level, args) => {
    if (logs.length >= ${CODE_LOG_MAX_ENTRIES} || logBytes >= ${CODE_LOG_MAX_BYTES}) return;
    let line;
    try {
      line = args.map((a) => (typeof a === "string" ? a : JSON.stringify(a))).join(" ");
    } catch {
      line = "[unserializable]";
    }
    line = level + ": " + String(line).slice(0, 2048);
    logBytes += line.length;
    logs.push(line);
  };
  globalThis.console = {
    log: (...a) => push("log", a),
    info: (...a) => push("info", a),
    warn: (...a) => push("warn", a),
    error: (...a) => push("error", a),
    debug: (...a) => push("debug", a),
  };
  globalThis.__logs = logs;
  const seen = new Set();
  const deepFreeze = (v) => {
    if (v === null || typeof v !== "object" || seen.has(v)) return v;
    seen.add(v);
    for (const k of Object.keys(v)) deepFreeze(v[k]);
    return Object.freeze(v);
  };
  globalThis.__ctx = deepFreeze(JSON.parse(globalThis.__ctxJson));
  delete globalThis.__ctxJson;
})()
`;

const GUEST_DRIVER = `
(() => {
  const contract = (message) => {
    const e = new Error(message);
    e.name = "ContractError";
    return e;
  };
  if (typeof globalThis.__default !== "function") {
    throw contract("code must export a default function");
  }
  const r = globalThis.__default(globalThis.__ctx);
  if (r === undefined) throw contract("code returned undefined; return a JSON value");
  if (r !== null && (typeof r === "object" || typeof r === "function") && typeof r.then === "function") {
    // An async default export returns a Promise, which JSON.stringify would
    // silently serialize as "{}". Reject it with a clear contract error; the
    // sandbox is synchronous by design (no job-queue drain).
    throw contract("async code is not supported; return a JSON value, not a Promise");
  }
  const s = JSON.stringify(r);
  if (s === undefined) throw contract("result is not JSON-serializable");
  if (s.length > ${CODE_OUTPUT_MAX_BYTES}) {
    const e = new Error("result exceeds ${CODE_OUTPUT_MAX_BYTES} bytes");
    e.name = "OutputError";
    throw e;
  }
  return s;
})()
`;

type CoreModule = typeof import("quickjs-emscripten-core");
type SyncVariant = typeof import("@jitl/quickjs-singlefile-mjs-release-sync").default;

let coreLoader: Promise<{ core: CoreModule; baseVariant: SyncVariant }> | undefined;

/** The base variant import is cached; each evaluation instantiates a fresh
 * module over a fresh capped memory from it. Lazy so boot pays nothing. */
function loadCore() {
  coreLoader ??= (async () => {
    const [core, variantModule] = await Promise.all([
      import("quickjs-emscripten-core"),
      import("@jitl/quickjs-singlefile-mjs-release-sync"),
    ]);
    return { core, baseVariant: variantModule.default };
  })();
  return coreLoader;
}

async function freshModule(): Promise<QuickJSWASMModule> {
  const { core, baseVariant } = await loadCore();
  const variant = core.newVariant(baseVariant, {
    wasmMemory: new WebAssembly.Memory({
      initial: WASM_MEMORY_INITIAL_PAGES,
      maximum: Math.ceil(WASM_MEMORY_MAX_BYTES / WASM_PAGE_BYTES),
    }),
  });
  return core.newQuickJSWASMModuleFromVariant(variant);
}

interface DumpedError {
  name?: unknown;
  message?: unknown;
  stack?: unknown;
}

function normalizeError(dumped: unknown): CodeErrorShape {
  const raw: DumpedError =
    typeof dumped === "object" && dumped !== null ? (dumped as DumpedError) : {};
  let name = typeof raw.name === "string" ? raw.name : "Error";
  let message = typeof raw.message === "string" ? raw.message : String(dumped ?? "unknown error");
  if (name === "InternalError" && message === "interrupted") {
    name = "TimeoutError";
    message = `exceeded the ${CODE_CPU_BUDGET_MS}ms CPU budget`;
  } else if (name === "InternalError" && message.includes("out of memory")) {
    name = "MemoryError";
    message = `exceeded the ${CODE_MEMORY_LIMIT_BYTES} byte memory limit`;
  } else if (name === "InternalError" && message.includes("stack overflow")) {
    name = "StackError";
    message = `exceeded the ${CODE_MAX_STACK_BYTES} byte stack limit`;
  }
  const shape: CodeErrorShape = { name, message };
  const stack = typeof raw.stack === "string" ? raw.stack : "";
  const fromStack = /automation\.js:(\d+)/.exec(stack);
  const fromMessage = /automation\.js:(\d+)/.exec(message);
  const line = fromStack?.[1] ?? fromMessage?.[1];
  if (line !== undefined) shape.line = Number(line);
  return shape;
}

function readLogs(ctx: QuickJSContext, track: (handle: QuickJSHandle) => QuickJSHandle): string[] {
  const result = ctx.evalCode(`JSON.stringify(globalThis.__logs ?? [])`);
  if (result.error) {
    track(result.error);
    return [];
  }
  const value = track(result.value);
  const parsed: unknown = JSON.parse(ctx.getString(value));
  return Array.isArray(parsed) ? parsed.filter((l): l is string => typeof l === "string") : [];
}

/** Evaluate user code against a frozen JSON input. Never throws for guest
 * failures — every contract, limit, or runtime problem is a typed outcome. */
export async function evaluateCode(
  source: string,
  input: CodeInput,
  mode: CodeMode,
): Promise<CodeOutcome> {
  const startedAt = performance.now();
  const duration = () => Math.round(performance.now() - startedAt);

  let runtime: QuickJSRuntime | undefined;
  let ctx: QuickJSContext | undefined;
  const handles: QuickJSHandle[] = [];
  const track = (handle: QuickJSHandle): QuickJSHandle => {
    handles.push(handle);
    return handle;
  };

  try {
    const quickjs = await freshModule();
    runtime = quickjs.newRuntime();
    runtime.setMemoryLimit(CODE_MEMORY_LIMIT_BYTES);
    runtime.setMaxStackSize(CODE_MAX_STACK_BYTES);
    const deadline = Date.now() + CODE_CPU_BUDGET_MS;
    runtime.setInterruptHandler(() => Date.now() >= deadline);
    ctx = runtime.newContext();

    // Stage the input as a string prop (no source-literal escaping), then run
    // the prelude: console shim + deep-frozen __ctx.
    const json = track(ctx.newString(JSON.stringify(input)));
    ctx.setProp(ctx.global, "__ctxJson", json);
    const prelude = ctx.evalCode(GUEST_PRELUDE);
    if (prelude.error) {
      track(prelude.error);
      return {
        ok: false,
        error: normalizeError(ctx.dump(prelude.error)),
        durationMs: duration(),
        logs: [],
      };
    }
    track(prelude.value);

    const moduleResult = ctx.evalCode(source, "automation.js", { type: "module" });
    if (moduleResult.error) {
      track(moduleResult.error);
      return {
        ok: false,
        error: normalizeError(ctx.dump(moduleResult.error)),
        durationMs: duration(),
        logs: readLogs(ctx, track),
      };
    }
    const ns = track(moduleResult.value);
    const defaultExport = track(ctx.getProp(ns, "default"));
    ctx.setProp(ctx.global, "__default", defaultExport);

    const driven = ctx.evalCode(GUEST_DRIVER);
    if (driven.error) {
      track(driven.error);
      return {
        ok: false,
        error: normalizeError(ctx.dump(driven.error)),
        durationMs: duration(),
        logs: readLogs(ctx, track),
      };
    }
    const serialized = ctx.getString(track(driven.value));
    const logs = readLogs(ctx, track);

    let value: unknown;
    try {
      value = JSON.parse(serialized);
    } catch {
      return {
        ok: false,
        error: { name: "ContractError", message: "result did not round-trip as JSON" },
        durationMs: duration(),
        logs,
      };
    }
    if (mode === "boolean" && value !== true && value !== false) {
      return {
        ok: false,
        error: { name: "ContractError", message: "boolean-mode code must return true or false" },
        durationMs: duration(),
        logs,
      };
    }
    return { ok: true, value, durationMs: duration(), logs };
  } catch (error) {
    // A host exception can bypass QuickJS unwinding. Abandon the fresh instance;
    // its native cleanup is unsafe, including handles acquired while reading logs.
    handles.length = 0;
    ctx = undefined;
    runtime = undefined;
    return {
      ok: false,
      error: {
        name:
          error instanceof RangeError &&
          /maximum call stack size exceeded|stack overflow/i.test(error.message)
            ? error.name
            : "SandboxError",
        message: error instanceof Error ? error.message : String(error),
      },
      durationMs: duration(),
      logs: [],
    };
  } finally {
    try {
      // Dumping a Promise exception can consume its handle.
      for (const handle of handles.reverse()) {
        if (handle.alive) handle.dispose();
      }
      ctx?.dispose();
      runtime?.dispose();
    } catch {
      // A failed cleanup also ends native access. The host collects the instance.
    }
  }
}
