/**
 * Pure span construction for the session-telemetry exporter: parsed
 * session-event payloads + session identity → OTel GenAI spans (semconv
 * `gen_ai.*`, which Langfuse maps natively to generations/observations).
 *
 * Everything here is deterministic and side-effect free — the golden tests
 * pin the exact OTLP JSON. Trace/span ids derive from stable identifiers
 * (task, session, run, tool call) plus the ADR 0028 recovery epoch, so a
 * replayed or re-exported timeline upserts rather than duplicating, and a
 * rewound-then-recovered segment gets DISTINCT span ids instead of
 * overlapping the pre-rewind ones.
 */

import {
  boolAttr,
  deterministicSpanId,
  deterministicTraceId,
  doubleAttr,
  intAttr,
  msToUnixNano,
  SPAN_KIND_CLIENT,
  SPAN_KIND_INTERNAL,
  STATUS_ERROR,
  STATUS_OK,
  strAttr,
  toUnixNano,
  type OtlpAttribute,
  type OtlpSpan,
  type OtlpSpanEvent,
} from "./otlp.ts";

/** Identity of the session's task, looked up once per session (cached). */
export interface SessionIdentity {
  taskId: string;
  rootTaskId: string | null;
  createdByUserId: string | null;
  harness: string | null;
  modelRouter: string | null;
  profileId: string | null;
  /** task.created_at — the session span's start time. */
  taskCreatedAtMs: number | null;
}

/** One parsed, run-scoped session event the builder consumes. */
export type RunEvent =
  | {
      type: "generation";
      at?: string;
      messageId: string;
      model: string;
      inputTokens: number;
      outputTokens: number;
      cacheReadTokens: number;
      cacheCreationTokens: number;
    }
  | { type: "run_cost"; at?: string; costMicroUsd: number }
  | {
      type: "tool_started";
      at?: string;
      toolCallId: string;
      toolName: string;
      argsSummary?: string;
    }
  | {
      type: "tool_completed";
      at?: string;
      toolCallId: string;
      toolName: string;
      ok: boolean;
      durationMs: number;
      resultSummary?: string;
    }
  | { type: "message"; at?: string; messageId: string; role: string; text: string };

/** Replay metadata the coordinator folds into every replayed payload. */
export interface ReplayMeta {
  rewound: boolean;
  epoch: number;
}

interface BasePayload {
  at?: string;
  run_id?: string;
  _rewound?: boolean;
  _recovery_epoch?: number;
}

function parse(payloadJson: string): (BasePayload & Record<string, unknown>) | undefined {
  try {
    const v: unknown = JSON.parse(payloadJson);
    return typeof v === "object" && v !== null
      ? (v as BasePayload & Record<string, unknown>)
      : undefined;
  } catch {
    return undefined;
  }
}

export function parseReplayMeta(payloadJson: string): ReplayMeta {
  const p = parse(payloadJson);
  return {
    rewound: p?._rewound === true,
    epoch: typeof p?._recovery_epoch === "number" ? p._recovery_epoch : 0,
  };
}

/** The payload's `run_id`, for buffering events under their turn. */
export function parseRunId(payloadJson: string): string | undefined {
  const p = parse(payloadJson);
  return typeof p?.run_id === "string" && p.run_id !== "" ? p.run_id : undefined;
}

/** The payload's coordinator-stamped `at`, where present. */
export function parseAt(payloadJson: string): string | undefined {
  const p = parse(payloadJson);
  return typeof p?.at === "string" ? p.at : undefined;
}

function num(v: unknown): number {
  return typeof v === "number" && Number.isFinite(v) ? v : 0;
}

function str(v: unknown): string {
  return typeof v === "string" ? v : "";
}

function optStr(v: unknown): string | undefined {
  return typeof v === "string" && v !== "" ? v : undefined;
}

/** Parse one run-scoped event into the shape the builder consumes; undefined
 *  for kinds the builder has no use for (stdout, prompt queue churn, …). */
export function parseRunEvent(kind: string, payloadJson: string): RunEvent | undefined {
  const p = parse(payloadJson);
  if (p === undefined) return undefined;
  switch (kind) {
    case "generation":
      return {
        type: "generation",
        at: p.at,
        messageId: str(p["message_id"]),
        model: str(p["model"]),
        inputTokens: num(p["input_tokens"]),
        outputTokens: num(p["output_tokens"]),
        cacheReadTokens: num(p["cache_read_tokens"]),
        cacheCreationTokens: num(p["cache_creation_tokens"]),
      };
    case "run_cost":
      return { type: "run_cost", at: p.at, costMicroUsd: num(p["cost_micro_usd"]) };
    case "tool_call_started":
      return {
        type: "tool_started",
        at: p.at,
        toolCallId: str(p["tool_call_id"]),
        toolName: str(p["tool_name"]),
        argsSummary: optStr(p["args_summary"]),
      };
    case "tool_call_completed":
      return {
        type: "tool_completed",
        at: p.at,
        toolCallId: str(p["tool_call_id"]),
        toolName: str(p["tool_name"]),
        ok: p["ok"] !== false,
        durationMs: num(p["duration_ms"]),
        resultSummary: optStr(p["result_summary"]),
      };
    case "agent_message":
      return {
        type: "message",
        at: p.at,
        messageId: str(p["message_id"]),
        role: str(p["role"]),
        text: str(p["text"]),
      };
    default:
      return undefined;
  }
}

/** A run ready to become spans: its boundary facts plus buffered events. */
export interface CompletedRun {
  runId: string;
  /** ADR 0028 recovery epoch this run's events were delivered under. */
  epoch: number;
  /** run_started payload `at`; undefined for a run adopted mid-flight. */
  startAt?: string;
  /** run_completed / run_interrupted `at`, or the exporter's fallback. */
  endAt?: string;
  promptId?: string;
  promptSummary?: string;
  interrupted: boolean;
  ok: boolean;
  events: RunEvent[];
}

export function traceIdFor(sessionId: string, identity: SessionIdentity | null): string {
  return deterministicTraceId(identity?.rootTaskId ?? identity?.taskId ?? sessionId);
}

export function sessionSpanIdFor(sessionId: string): string {
  return deterministicSpanId(`${sessionId}:session:0`);
}

/** Max content chars carried on one span attribute (bounds sink payloads). */
const MAX_CONTENT_CHARS = 8_192;

function clip(text: string): string {
  return text.length > MAX_CONTENT_CHARS ? text.slice(0, MAX_CONTENT_CHARS) : text;
}

/**
 * Build the turn span plus its generation/tool children (and message span
 * events) for one completed run. Every timestamp fallback is deliberate: a
 * run adopted mid-flight (listener started late) may lack `startAt`, and a
 * flushed-on-terminal run may lack `endAt` — the spans still export, just
 * with a degenerate duration, rather than dropping spent tokens.
 */
export function buildRunSpans(opts: {
  sessionId: string;
  identity: SessionIdentity | null;
  run: CompletedRun;
  captureContent: boolean;
  /** Fallback timestamp (epoch ms) when the run carries no usable `at`. */
  fallbackMs: number;
}): OtlpSpan[] {
  const { sessionId, identity, run, captureContent } = opts;
  const traceId = traceIdFor(sessionId, identity);
  const turnSpanId = deterministicSpanId(`${sessionId}:run:${run.runId}:${run.epoch}`);
  const fallbackNano = msToUnixNano(opts.fallbackMs);
  const endNano = toUnixNano(run.endAt) ?? fallbackNano;
  const startNano = toUnixNano(run.startAt) ?? firstEventNano(run.events) ?? endNano;

  const spans: OtlpSpan[] = [];
  const turnEvents: OtlpSpanEvent[] = [];
  const turnAttributes: OtlpAttribute[] = [
    strAttr("gen_ai.operation.name", "invoke_agent"),
    strAttr("session.id", sessionId),
    strAttr("engrams.run_id", run.runId),
  ];
  if (run.promptId) turnAttributes.push(strAttr("engrams.prompt_id", run.promptId));
  if (captureContent && run.promptSummary) {
    turnAttributes.push(strAttr("gen_ai.prompt", clip(run.promptSummary)));
  }
  if (run.interrupted) turnAttributes.push(boolAttr("engrams.interrupted", true));
  if (run.epoch !== 0) turnAttributes.push(intAttr("engrams.recovery_epoch", run.epoch));

  // Join tool_started args onto the completed span; a started with no
  // completed (run ended mid-call) contributes nothing.
  const argsByCall = new Map<string, string>();
  for (const ev of run.events) {
    if (ev.type === "tool_started" && ev.argsSummary) {
      argsByCall.set(ev.toolCallId, ev.argsSummary);
    }
  }

  for (const [i, ev] of run.events.entries()) {
    switch (ev.type) {
      case "generation": {
        const genEnd = toUnixNano(ev.at) ?? endNano;
        const attributes: OtlpAttribute[] = [
          strAttr("gen_ai.operation.name", "chat"),
          intAttr("gen_ai.usage.input_tokens", ev.inputTokens),
          intAttr("gen_ai.usage.output_tokens", ev.outputTokens),
          intAttr("gen_ai.usage.cache_read_input_tokens", ev.cacheReadTokens),
          intAttr("gen_ai.usage.cache_creation_input_tokens", ev.cacheCreationTokens),
        ];
        if (ev.model) attributes.push(strAttr("gen_ai.request.model", ev.model));
        if (ev.messageId) attributes.push(strAttr("gen_ai.response.id", ev.messageId));
        spans.push({
          traceId,
          spanId: deterministicSpanId(
            `${sessionId}:gen:${ev.messageId || `${run.runId}#${i}`}:${run.epoch}`,
          ),
          parentSpanId: turnSpanId,
          name: ev.model ? `chat ${ev.model}` : "chat",
          kind: SPAN_KIND_CLIENT,
          startTimeUnixNano: startNano,
          endTimeUnixNano: genEnd,
          attributes,
        });
        break;
      }
      case "run_cost": {
        // Cost is a turn-level fact (the agent reports it once per run).
        turnAttributes.push(doubleAttr("engrams.cost_usd", ev.costMicroUsd / 1_000_000));
        break;
      }
      case "tool_completed": {
        const toolEnd = toUnixNano(ev.at) ?? endNano;
        // The harness-measured duration is the honest one; deriving start
        // from it beats pairing with tool_call_started's transit-skewed at.
        const toolStart =
          ev.durationMs > 0
            ? (BigInt(toolEnd) - BigInt(ev.durationMs) * 1_000_000n).toString()
            : toolEnd;
        const attributes: OtlpAttribute[] = [
          strAttr("gen_ai.operation.name", "execute_tool"),
          strAttr("gen_ai.tool.name", ev.toolName),
          strAttr("gen_ai.tool.call.id", ev.toolCallId),
        ];
        if (captureContent) {
          const args = argsByCall.get(ev.toolCallId);
          if (args) attributes.push(strAttr("engrams.tool.args_summary", clip(args)));
          if (ev.resultSummary) {
            attributes.push(strAttr("engrams.tool.result_summary", clip(ev.resultSummary)));
          }
        }
        spans.push({
          traceId,
          spanId: deterministicSpanId(`${sessionId}:tool:${ev.toolCallId}:${run.epoch}`),
          parentSpanId: turnSpanId,
          name: `execute_tool ${ev.toolName}`.trim(),
          kind: SPAN_KIND_INTERNAL,
          startTimeUnixNano: toolStart,
          endTimeUnixNano: toolEnd,
          attributes,
          status: ev.ok ? { code: STATUS_OK } : { code: STATUS_ERROR },
        });
        break;
      }
      case "message": {
        const attributes: OtlpAttribute[] = [strAttr("gen_ai.message.role", ev.role)];
        if (captureContent && ev.text) {
          attributes.push(strAttr("gen_ai.message.content", clip(ev.text)));
        }
        turnEvents.push({
          timeUnixNano: toUnixNano(ev.at) ?? endNano,
          name: "agent_message",
          attributes,
        });
        break;
      }
      case "tool_started":
        break; // joined onto tool_completed above
    }
  }

  spans.push({
    traceId,
    spanId: turnSpanId,
    parentSpanId: sessionSpanIdFor(sessionId),
    name: "agent_turn",
    kind: SPAN_KIND_INTERNAL,
    startTimeUnixNano: startNano,
    endTimeUnixNano: endNano,
    attributes: turnAttributes,
    ...(turnEvents.length > 0 ? { events: turnEvents } : {}),
    status: run.ok && !run.interrupted ? { code: STATUS_OK } : { code: STATUS_ERROR },
  });
  return spans;
}

/** The session root span, emitted once at terminal. */
export function buildSessionSpan(opts: {
  sessionId: string;
  identity: SessionIdentity | null;
  outcome: "completed" | "failed" | "neutral";
  endMs: number;
}): OtlpSpan {
  const { sessionId, identity, outcome } = opts;
  const endNano = msToUnixNano(opts.endMs);
  const attributes: OtlpAttribute[] = [
    strAttr("gen_ai.operation.name", "invoke_agent"),
    strAttr("session.id", sessionId),
    strAttr("engrams.session.outcome", outcome),
  ];
  if (identity) {
    attributes.push(strAttr("engrams.task.id", identity.taskId));
    if (identity.rootTaskId) attributes.push(strAttr("engrams.root_task.id", identity.rootTaskId));
    if (identity.createdByUserId) attributes.push(strAttr("user.id", identity.createdByUserId));
    if (identity.harness) attributes.push(strAttr("engrams.harness", identity.harness));
    if (identity.modelRouter) attributes.push(strAttr("engrams.model_router", identity.modelRouter));
    if (identity.profileId) attributes.push(strAttr("engrams.profile_id", identity.profileId));
  }
  return {
    traceId: traceIdFor(sessionId, identity),
    spanId: sessionSpanIdFor(sessionId),
    name: "agent_session",
    kind: SPAN_KIND_INTERNAL,
    startTimeUnixNano:
      identity?.taskCreatedAtMs != null ? msToUnixNano(identity.taskCreatedAtMs) : endNano,
    endTimeUnixNano: endNano,
    attributes,
    status: outcome === "failed" ? { code: STATUS_ERROR } : { code: STATUS_OK },
  };
}

function firstEventNano(events: RunEvent[]): string | undefined {
  for (const ev of events) {
    const nano = toUnixNano(ev.at);
    if (nano !== undefined) return nano;
  }
  return undefined;
}
