/**
 * Live request tracing for the orchestrator: spans for the work it does now,
 * exported over OTLP/HTTP JSON to the same collector the Rust services use,
 * and W3C `traceparent` propagation in and out, so one user request is one
 * trace from here through the coordinator and the host.
 *
 * The current span rides an AsyncLocalStorage, so it follows every await of
 * the request that opened it. Tracing is off until `initTracing` gets an
 * endpoint; while off, `inSpan` only runs its callback and nothing is sent.
 *
 * This is separate from span-builder.ts, which rebuilds GenAI spans from the
 * persisted event log after the fact.
 */

import { AsyncLocalStorage } from "node:async_hooks";
import { randomBytes } from "node:crypto";

import { log } from "../log.ts";
import {
  buildExportRequest,
  strAttr,
  SPAN_KIND_CLIENT,
  SPAN_KIND_INTERNAL,
  SPAN_KIND_SERVER,
  STATUS_ERROR,
  STATUS_OK,
  type OtlpAttribute,
  type OtlpSpan,
} from "./otlp.ts";

export { SPAN_KIND_CLIENT, SPAN_KIND_SERVER };

export interface SpanContext {
  traceId: string;
  spanId: string;
}

export interface SpanOptions {
  kind?: number;
  attributes?: Record<string, string | number | boolean | undefined>;
  /** A remote parent (an inbound `traceparent`); default the current span. */
  parent?: SpanContext;
}

/** The handle `inSpan` passes to its callback. */
export interface ActiveSpan {
  readonly context: SpanContext;
  setAttribute(key: string, value: string | number | boolean): void;
}

/** Where finished spans go. */
export interface SpanSink {
  push(span: OtlpSpan): void;
  flush(): Promise<void>;
}

const current = new AsyncLocalStorage<SpanContext>();

/** Wall clock in epoch milliseconds, with sub-millisecond precision. */
export function nowMs(): number {
  return performance.timeOrigin + performance.now();
}

function toNano(ms: number): string {
  return BigInt(Math.round(ms * 1_000_000)).toString();
}

function attributes(attrs: SpanOptions["attributes"]): OtlpAttribute[] {
  const out: OtlpAttribute[] = [];
  for (const [key, value] of Object.entries(attrs ?? {})) {
    if (value === undefined) continue;
    out.push(attribute(key, value));
  }
  return out;
}

function attribute(key: string, value: string | number | boolean): OtlpAttribute {
  if (typeof value === "boolean") return { key, value: { boolValue: value } };
  if (typeof value === "number") {
    return Number.isInteger(value)
      ? { key, value: { intValue: String(value) } }
      : { key, value: { doubleValue: value } };
  }
  return strAttr(key, value);
}

const TRACEPARENT = /^00-([0-9a-f]{32})-([0-9a-f]{16})-[0-9a-f]{2}$/;

/** Parse a W3C `traceparent`; undefined for anything malformed or all-zero. */
export function parseTraceparent(header: string | null | undefined): SpanContext | undefined {
  const match = header ? TRACEPARENT.exec(header.trim()) : null;
  const traceId = match?.[1];
  const spanId = match?.[2];
  if (!traceId || !spanId) return undefined;
  if (/^0+$/.test(traceId) || /^0+$/.test(spanId)) return undefined;
  return { traceId, spanId };
}

export function formatTraceparent(ctx: SpanContext): string {
  return `00-${ctx.traceId}-${ctx.spanId}-01`;
}

export class Tracer {
  readonly #sink: SpanSink | null;

  constructor(sink: SpanSink | null) {
    this.#sink = sink;
  }

  get enabled(): boolean {
    return this.#sink !== null;
  }

  /** The current span's `traceparent`, for an outbound request. */
  traceparent(): string | undefined {
    const ctx = current.getStore();
    return this.#sink && ctx ? formatTraceparent(ctx) : undefined;
  }

  /** Run `fn` in a new span, a child of `options.parent` or the current span. */
  async inSpan<T>(
    name: string,
    options: SpanOptions,
    fn: (span: ActiveSpan) => Promise<T> | T,
  ): Promise<T> {
    const sink = this.#sink;
    if (!sink) return fn(DISABLED_SPAN);
    const parent = options.parent ?? current.getStore();
    const context: SpanContext = {
      traceId: parent?.traceId ?? randomBytes(16).toString("hex"),
      spanId: randomBytes(8).toString("hex"),
    };
    const attrs = attributes(options.attributes);
    const span: ActiveSpan = {
      context,
      setAttribute: (key, value) => attrs.push(attribute(key, value)),
    };
    const start = nowMs();
    let error: unknown;
    try {
      return await current.run(context, () => fn(span));
    } catch (err) {
      error = err;
      throw err;
    } finally {
      sink.push({
        traceId: context.traceId,
        spanId: context.spanId,
        ...(parent ? { parentSpanId: parent.spanId } : {}),
        name,
        kind: options.kind ?? SPAN_KIND_INTERNAL,
        startTimeUnixNano: toNano(start),
        endTimeUnixNano: toNano(nowMs()),
        attributes: attrs,
        status:
          error === undefined
            ? { code: STATUS_OK }
            : { code: STATUS_ERROR, message: String(error) },
      });
    }
  }

  /** Record a span that already happened (a wait measured after the fact),
   * as a child of the current span. */
  record(
    name: string,
    startMs: number,
    endMs: number,
    attrs?: SpanOptions["attributes"],
  ): void {
    const sink = this.#sink;
    const parent = current.getStore();
    if (!sink || !parent) return;
    sink.push({
      traceId: parent.traceId,
      spanId: randomBytes(8).toString("hex"),
      parentSpanId: parent.spanId,
      name,
      kind: SPAN_KIND_INTERNAL,
      startTimeUnixNano: toNano(startMs),
      endTimeUnixNano: toNano(endMs),
      attributes: attributes(attrs),
    });
  }

  flush(): Promise<void> {
    return this.#sink?.flush() ?? Promise.resolve();
  }
}

const DISABLED_SPAN: ActiveSpan = {
  context: { traceId: "0".repeat(32), spanId: "0".repeat(16) },
  setAttribute: () => {},
};

const BATCH_MAX = 256;
const FLUSH_INTERVAL_MS = 2_000;

/** Batches spans and posts them to `<endpoint>/v1/traces` as OTLP/HTTP JSON.
 * A failed post drops its batch: tracing never holds back a request. */
export function makeOtlpHttpSink(
  endpoint: string,
  serviceName: string,
  post: (url: string, init: RequestInit) => Promise<Response> = fetch,
): SpanSink {
  const url = endpoint.replace(/\/+$/, "").endsWith("/v1/traces")
    ? endpoint
    : `${endpoint.replace(/\/+$/, "")}/v1/traces`;
  const resource = [strAttr("service.name", serviceName)];
  let buffer: OtlpSpan[] = [];
  let failing = false;
  const flush = async () => {
    if (buffer.length === 0) return;
    const batch = buffer;
    buffer = [];
    try {
      const response = await post(url, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(buildExportRequest(resource, batch, "engrams-orchestrator")),
      });
      if (!response.ok) throw new Error(`OTLP export answered ${response.status}`);
      failing = false;
    } catch (err) {
      if (!failing) log.warn({ err, url }, "trace export failed; dropping spans");
      failing = true;
    }
  };
  const timer = setInterval(() => void flush(), FLUSH_INTERVAL_MS);
  timer.unref?.();
  return {
    push(span) {
      buffer.push(span);
      if (buffer.length >= BATCH_MAX) void flush();
    },
    flush,
  };
}

/** The process tracer. Off until `initTracing` gets an endpoint. */
export let tracer = new Tracer(null);

/** Turn tracing on when `OTEL_EXPORTER_OTLP_ENDPOINT` names an OTLP/HTTP
 * collector (for example `http://localhost:4318`). */
export function initTracing(endpoint: string | undefined, serviceName = "engrams-orchestrator"): void {
  if (!endpoint) return;
  tracer = new Tracer(makeOtlpHttpSink(endpoint, serviceName));
  log.info({ endpoint }, "OTLP trace export enabled");
}

/** Install a tracer directly (tests). */
export function setTracer(next: Tracer): void {
  tracer = next;
}
