/**
 * Minimal OTLP/HTTP-JSON encoding for the session-telemetry exporter.
 *
 * Hand-rolled on purpose (no @opentelemetry/* dependency): the exporter
 * constructs spans post-hoc from a persisted event log with explicit
 * historical timestamps and DETERMINISTIC ids, which the OTel JS SDK fights
 * (HrTime tuples, random-by-design id generators, live-span lifecycle). The
 * OTLP/JSON `ExportTraceServiceRequest` shape is small, stable, and
 * spec-mandated for any OTLP/HTTP server; Langfuse ingests it at
 * `/api/public/otel/v1/traces`.
 *
 * Encoding rules the golden tests pin (the OTLP/JSON spec's deviations from
 * plain proto3-JSON):
 *  - field names are lowerCamelCase;
 *  - `startTimeUnixNano` / `endTimeUnixNano` / `timeUnixNano` are DECIMAL
 *    STRINGS (u64 doesn't fit a JS number);
 *  - `traceId` / `spanId` are HEX strings (the spec's explicit exception to
 *    proto3-JSON's base64 bytes).
 */

import { createHash } from "node:crypto";

export interface OtlpAnyValue {
  stringValue?: string;
  boolValue?: boolean;
  intValue?: string;
  doubleValue?: number;
}

export interface OtlpAttribute {
  key: string;
  value: OtlpAnyValue;
}

export interface OtlpSpanEvent {
  timeUnixNano: string;
  name: string;
  attributes?: OtlpAttribute[];
}

/** proto `opentelemetry.proto.trace.v1.Span.SpanKind`. */
export const SPAN_KIND_INTERNAL = 1;
export const SPAN_KIND_CLIENT = 3;

/** proto `opentelemetry.proto.trace.v1.Status.StatusCode`. */
export const STATUS_UNSET = 0;
export const STATUS_OK = 1;
export const STATUS_ERROR = 2;

export interface OtlpSpan {
  traceId: string;
  spanId: string;
  parentSpanId?: string;
  name: string;
  kind: number;
  startTimeUnixNano: string;
  endTimeUnixNano: string;
  attributes?: OtlpAttribute[];
  events?: OtlpSpanEvent[];
  status?: { code: number; message?: string };
}

export interface ExportTraceServiceRequest {
  resourceSpans: Array<{
    resource: { attributes: OtlpAttribute[] };
    scopeSpans: Array<{
      scope: { name: string };
      spans: OtlpSpan[];
    }>;
  }>;
}

export function strAttr(key: string, value: string): OtlpAttribute {
  return { key, value: { stringValue: value } };
}

export function intAttr(key: string, value: number | bigint): OtlpAttribute {
  return { key, value: { intValue: String(value) } };
}

export function doubleAttr(key: string, value: number): OtlpAttribute {
  return { key, value: { doubleValue: value } };
}

export function boolAttr(key: string, value: boolean): OtlpAttribute {
  return { key, value: { boolValue: value } };
}

/**
 * Deterministic 16-byte trace id (32 hex chars) from a stable seed. The same
 * task exports the same trace id on every (re-)export, so at-least-once
 * delivery upserts instead of duplicating.
 */
export function deterministicTraceId(seed: string): string {
  return createHash("sha256").update(`engrams:trace:${seed}`).digest("hex").slice(0, 32);
}

/** Deterministic 8-byte span id (16 hex chars) from a stable seed. */
export function deterministicSpanId(seed: string): string {
  return createHash("sha256").update(`engrams:span:${seed}`).digest("hex").slice(0, 16);
}

/**
 * RFC3339 `at` (the coordinator-stamped payload timestamp) → OTLP nanosecond
 * decimal string. Returns undefined for a missing/unparseable timestamp; the
 * caller picks its fallback.
 */
export function toUnixNano(at: string | undefined): string | undefined {
  if (!at) return undefined;
  const ms = Date.parse(at);
  if (Number.isNaN(ms)) return undefined;
  return (BigInt(ms) * 1_000_000n).toString();
}

/** Nanosecond decimal string from an epoch-milliseconds number. */
export function msToUnixNano(ms: number): string {
  return (BigInt(Math.round(ms)) * 1_000_000n).toString();
}

export function buildExportRequest(
  resourceAttributes: OtlpAttribute[],
  spans: OtlpSpan[],
): ExportTraceServiceRequest {
  return {
    resourceSpans: [
      {
        resource: { attributes: resourceAttributes },
        scopeSpans: [
          {
            scope: { name: "engrams-session-telemetry" },
            spans,
          },
        ],
      },
    ],
  };
}
