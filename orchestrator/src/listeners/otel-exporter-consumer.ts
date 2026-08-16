/**
 * Session-telemetry exporter: a RAW session consumer that turns the persisted
 * event log into OTel GenAI spans and posts them to one configured OTLP sink
 * (Langfuse or any collector).
 *
 * One consumer instance per (session, sink): the production factory returns
 * one consumer per `config.telemetry` sink, each named
 * `otel-exporter:<sink.name>` so it owns an independent durable cursor,
 * retry budget, and buffers — a dead sink stalls only its own export
 * progress, never another sink and never the workload (the consumer sits
 * strictly downstream of the persisted log).
 *
 * Delivery is at-least-once with DETERMINISTIC span ids (span-builder.ts), so
 * a replay after a crash re-posts byte-identical spans and the backend
 * upserts. The commit-idx protocol holds the durable cursor at the earliest
 * still-buffered run while a turn is open (`handle` returns the held floor),
 * so a restart replays the whole open run rather than losing its head.
 *
 * Epoch bookkeeping (ADR 0028): replayed payloads carry `_recovery_epoch`,
 * but LIVE frames always say 0 (events.rs merged_to_parts), so the consumer
 * tracks the current epoch itself — max of replayed epochs and
 * `recovered_from_checkpoint.recovery_epoch` — and stamps buffered runs with
 * it. Tombstoned (`_rewound`) events are skipped entirely.
 */

import { eq } from "drizzle-orm";

import type { TelemetryConfig, TelemetrySink } from "../config.ts";
import type { CuratedEvent } from "../control-plane/session-events.ts";
import { getDb } from "../db/client.ts";
import { task as taskTable, taskSession as taskSessionTable } from "../db/schema.ts";
import { log as rootLog } from "../log.ts";
import { buildExportRequest, strAttr, type ExportTraceServiceRequest, type OtlpSpan } from "../telemetry/otlp.ts";
import {
  buildRunSpans,
  buildSessionSpan,
  parseAt,
  parseReplayMeta,
  parseRunEvent,
  parseRunId,
  type CompletedRun,
  type RunEvent,
  type SessionIdentity,
} from "../telemetry/span-builder.ts";
import type { SessionConsumer } from "./consumer.ts";

const log = rootLog.child({ component: "otel-exporter" });

/** Max POST attempts per event before the flush is dropped (skip-and-advance,
 *  the title-consumer policy: the pump's 250ms→30s backoff spaces them). */
const MAX_EXPORT_ATTEMPTS = 8;
/** Early-flush threshold: a run buffering more payload than this posts its
 *  children now and keeps only the run metadata (memory bound; the final
 *  flush re-posts the turn span with complete data — same ids, upsert). */
const MAX_RUN_BUFFER_BYTES = 256 * 1024;
/** Early-flush threshold on buffer age, checked as events arrive. */
const MAX_RUN_BUFFER_AGE_MS = 5 * 60_000;

export interface OtelExporterDeps {
  lookupSessionIdentity(sessionId: string): Promise<SessionIdentity | null>;
  /** POST one OTLP request to the sink. Throws on failure (any non-2xx). */
  post(sink: TelemetrySink, body: ExportTraceServiceRequest): Promise<void>;
  nowMs?: () => number;
}

interface RunState {
  runId: string;
  /** idx of the first event buffered for this run — the commit floor. */
  firstIdx: bigint;
  epoch: number;
  startAt?: string;
  promptId?: string;
  promptSummary?: string;
  events: RunEvent[];
  approxBytes: number;
  firstBufferedMs: number;
}

export function makeOtelExporterConsumer(
  deps: OtelExporterDeps,
  sink: TelemetrySink,
): SessionConsumer {
  const nowMs = deps.nowMs ?? Date.now;
  const resourceAttributes = [
    strAttr("service.name", sink.serviceName),
    strAttr("telemetry.sdk.name", "engrams-session-telemetry"),
  ];

  // Per-instance = per-session state (the manager constructs fresh consumers
  // for every acquired session listener).
  let identity: SessionIdentity | null = null;
  let identityLoaded = false;
  const runs = new Map<string, RunState>();
  let epoch = 0;
  let lastAt: string | undefined;
  // Retry budget, keyed on the event idx being delivered (title-consumer
  // pattern: in-memory on purpose — a listener restart grants a fresh budget).
  let attemptEventIdx = -1n;
  let attempts = 0;
  // The session root span carries the trace identity (user.id, task,
  // harness, profile) and, in Langfuse, the trace name. Emit a PROVISIONAL
  // version with the FIRST flush so live traces are attributed from their
  // first turn — engrams sessions run for days, and a terminal-only root
  // left every live trace anonymous. The terminal flush re-emits the same
  // deterministic span id with final end time + outcome (an upsert).
  let sessionSpanPosted = false;

  function withSessionSpan(sessionId: string, spans: OtlpSpan[]): OtlpSpan[] {
    if (sessionSpanPosted || spans.length === 0) return spans;
    return [
      buildSessionSpan({ sessionId, identity, endMs: nowMs() }),
      ...spans,
    ];
  }

  /** Flush with the provisional session root prepended on the first
   *  successful POST (and only marked posted when it actually went out). */
  async function postFlush(
    eventIdx: bigint,
    sessionId: string,
    spans: OtlpSpan[],
  ): Promise<boolean> {
    const withRoot = withSessionSpan(sessionId, spans);
    const ok = await postWithBudget(eventIdx, withRoot);
    if (ok && withRoot.length > spans.length) sessionSpanPosted = true;
    return ok;
  }

  function commitFloor(eventIdx: bigint): bigint {
    let floor: bigint | null = null;
    for (const run of runs.values()) {
      if (floor === null || run.firstIdx < floor) floor = run.firstIdx;
    }
    return floor === null ? eventIdx : floor - 1n;
  }

  /** POST with the per-event retry budget: transient failures rethrow (the
   *  pump redelivers with backoff) until the budget is spent, then the flush
   *  is dropped so one dead sink can't wedge the cursor forever. Returns
   *  false when the flush was dropped. */
  async function postWithBudget(eventIdx: bigint, spans: OtlpSpan[]): Promise<boolean> {
    if (spans.length === 0) return true;
    try {
      await deps.post(sink, buildExportRequest(resourceAttributes, spans));
      return true;
    } catch (err) {
      if (eventIdx !== attemptEventIdx) {
        attemptEventIdx = eventIdx;
        attempts = 0;
      }
      attempts += 1;
      if (attempts < MAX_EXPORT_ATTEMPTS) {
        throw err;
      }
      // Deliberately no header/body in the log — sink headers are secrets.
      log.warn(
        { sink: sink.name, eventIdx: String(eventIdx), attempts, err },
        "otel export failed repeatedly; dropping this flush and advancing",
      );
      return false;
    }
  }

  function completedRunFrom(run: RunState, opts: {
    endAt?: string;
    ok: boolean;
    interrupted: boolean;
  }): CompletedRun {
    return {
      runId: run.runId,
      epoch: run.epoch,
      ...(run.startAt !== undefined ? { startAt: run.startAt } : {}),
      ...(opts.endAt !== undefined ? { endAt: opts.endAt } : {}),
      ...(run.promptId !== undefined ? { promptId: run.promptId } : {}),
      ...(run.promptSummary !== undefined ? { promptSummary: run.promptSummary } : {}),
      interrupted: opts.interrupted,
      ok: opts.ok,
      events: run.events,
    };
  }

  function bufferRunEvent(event: CuratedEvent, runId: string): RunState {
    let run = runs.get(runId);
    if (run === undefined) {
      // A run adopted mid-flight (listener started after run_started, or the
      // start predates the cursor). Buffer from here; spans degrade to the
      // first event's timestamp.
      run = {
        runId,
        firstIdx: event.idx,
        epoch,
        events: [],
        approxBytes: 0,
        firstBufferedMs: nowMs(),
      };
      runs.set(runId, run);
    }
    return run;
  }

  return {
    name: `otel-exporter:${sink.name}`,
    raw: true,
    // Cursor writes happen even for uninterested kinds, so a selective
    // interest would advance the cursor past a buffered run's head. Take
    // everything and no-op cheaply instead.
    interestedIn: () => true,
    async appliesTo(sessionId) {
      identity = await deps.lookupSessionIdentity(sessionId);
      identityLoaded = true;
      return identity !== null;
    },
    async handle(event, ctx) {
      if (!identityLoaded) {
        identity = await deps.lookupSessionIdentity(ctx.sessionId);
        identityLoaded = true;
      }
      const meta = parseReplayMeta(event.payloadJson);
      if (meta.rewound) return commitFloor(event.idx);
      if (meta.epoch > epoch) epoch = meta.epoch;
      const at = parseAt(event.payloadJson);
      if (at !== undefined) lastAt = at;

      switch (event.kind) {
        case "recovered_from_checkpoint": {
          // Live frames always carry `_recovery_epoch: 0`; this event is the
          // live-arm signal that the epoch advanced.
          try {
            const p = JSON.parse(event.payloadJson) as { recovery_epoch?: unknown };
            if (typeof p.recovery_epoch === "number" && p.recovery_epoch > epoch) {
              epoch = p.recovery_epoch;
            }
          } catch {
            // malformed payload — keep the tracked epoch
          }
          return commitFloor(event.idx);
        }
        case "run_started": {
          let promptId: string | undefined;
          let promptSummary: string | undefined;
          const runId = parseRunId(event.payloadJson);
          if (runId === undefined) return commitFloor(event.idx);
          try {
            const p = JSON.parse(event.payloadJson) as {
              prompt_id?: unknown;
              prompt_summary?: unknown;
            };
            if (typeof p.prompt_id === "string") promptId = p.prompt_id;
            if (typeof p.prompt_summary === "string") promptSummary = p.prompt_summary;
          } catch {
            // malformed payload — start the run without prompt facts
          }
          runs.set(runId, {
            runId,
            firstIdx: event.idx,
            epoch,
            ...(at !== undefined ? { startAt: at } : {}),
            ...(promptId !== undefined ? { promptId } : {}),
            ...(promptSummary !== undefined ? { promptSummary } : {}),
            events: [],
            approxBytes: 0,
            firstBufferedMs: nowMs(),
          });
          return commitFloor(event.idx);
        }
        case "generation":
        case "run_cost":
        case "tool_call_started":
        case "tool_call_completed":
        case "agent_message": {
          const runId = parseRunId(event.payloadJson);
          if (runId === undefined) return commitFloor(event.idx);
          const parsed = parseRunEvent(event.kind, event.payloadJson);
          if (parsed === undefined) return commitFloor(event.idx);
          const run = bufferRunEvent(event, runId);
          run.events.push(parsed);
          run.approxBytes += event.payloadJson.length;
          if (
            run.approxBytes > MAX_RUN_BUFFER_BYTES ||
            nowMs() - run.firstBufferedMs > MAX_RUN_BUFFER_AGE_MS
          ) {
            // Bound memory on a marathon turn: post what we have (children +
            // a provisional turn span), then keep only the run metadata. The
            // final flush re-posts the turn span with complete data — same
            // deterministic ids, so the sink upserts. The commit floor does
            // NOT advance: a crash replays the run from its head, and the
            // re-post is idempotent.
            const spans = buildRunSpans({
              sessionId: ctx.sessionId,
              identity,
              run: completedRunFrom(run, { ok: true, interrupted: false }),
              captureContent: sink.captureContent,
              fallbackMs: nowMs(),
            });
            if (await postFlush(event.idx, ctx.sessionId, spans)) {
              run.events = [];
              run.approxBytes = 0;
              run.firstBufferedMs = nowMs();
            }
          }
          return commitFloor(event.idx);
        }
        case "run_completed":
        case "run_interrupted": {
          const runId = parseRunId(event.payloadJson);
          const run = runId === undefined ? undefined : runs.get(runId);
          if (runId === undefined || run === undefined) return commitFloor(event.idx);
          let ok = false;
          if (event.kind === "run_completed") {
            try {
              ok = (JSON.parse(event.payloadJson) as { ok?: unknown }).ok === true;
            } catch {
              ok = false;
            }
          }
          const spans = buildRunSpans({
            sessionId: ctx.sessionId,
            identity,
            run: completedRunFrom(run, {
              ...(at !== undefined ? { endAt: at } : {}),
              ok,
              interrupted: event.kind === "run_interrupted",
            }),
            captureContent: sink.captureContent,
            fallbackMs: nowMs(),
          });
          await postFlush(event.idx, ctx.sessionId, spans);
          // Success and budget exhaustion both release the run: either the
          // spans are exported, or we deliberately gave up on them.
          runs.delete(runId);
          return commitFloor(event.idx);
        }
        default:
          return commitFloor(event.idx);
      }
    },
    async onTerminal(outcome, ctx) {
      // Flush every still-open run (end = the last coordinator timestamp
      // seen), then the session root span. onTerminal has no pump retry, so
      // failures are bounded-retried here and then logged — telemetry never
      // blocks the listener's terminal path.
      const spans: OtlpSpan[] = [];
      for (const run of runs.values()) {
        spans.push(
          ...buildRunSpans({
            sessionId: ctx.sessionId,
            identity,
            run: completedRunFrom(run, {
              ...(lastAt !== undefined ? { endAt: lastAt } : {}),
              ok: false,
              interrupted: true,
            }),
            captureContent: sink.captureContent,
            fallbackMs: nowMs(),
          }),
        );
      }
      runs.clear();
      const endMsFromLastAt = lastAt !== undefined ? Date.parse(lastAt) : Number.NaN;
      spans.push(
        buildSessionSpan({
          sessionId: ctx.sessionId,
          identity,
          outcome,
          endMs: Number.isNaN(endMsFromLastAt) ? nowMs() : endMsFromLastAt,
        }),
      );
      for (let attempt = 1; attempt <= 3; attempt++) {
        try {
          await deps.post(sink, buildExportRequest(resourceAttributes, spans));
          return;
        } catch (err) {
          if (attempt === 3) {
            log.warn(
              { sink: sink.name, sessionId: ctx.sessionId, err },
              "otel terminal flush failed; giving up",
            );
            return;
          }
          await Bun.sleep(1_000 * attempt);
        }
      }
    },
  };
}

/** One production consumer per configured sink; [] when telemetry is off, so
 *  the manager registers nothing and the feature has zero cost. */
export function makeProductionOtelExporterConsumers(
  telemetry: TelemetryConfig | undefined,
): SessionConsumer[] {
  if (telemetry === undefined || telemetry.sinks.length === 0) return [];
  const db = getDb();
  const deps: OtelExporterDeps = {
    async lookupSessionIdentity(sessionId) {
      const rows = await db
        .select({
          taskId: taskTable.id,
          rootTaskId: taskTable.rootTaskId,
          createdByUserId: taskTable.createdByUserId,
          harness: taskTable.harness,
          modelRouter: taskTable.modelRouter,
          profileId: taskSessionTable.profileId,
          createdAt: taskTable.createdAt,
        })
        .from(taskSessionTable)
        .innerJoin(taskTable, eq(taskSessionTable.taskId, taskTable.id))
        .where(eq(taskSessionTable.sessionId, sessionId))
        .limit(1);
      const row = rows[0];
      if (row === undefined) return null;
      return {
        taskId: row.taskId,
        rootTaskId: row.rootTaskId,
        createdByUserId: row.createdByUserId,
        harness: row.harness,
        modelRouter: row.modelRouter,
        profileId: row.profileId,
        taskCreatedAtMs: row.createdAt?.getTime() ?? null,
      };
    },
    async post(sink, body) {
      const response = await fetch(sink.endpoint, {
        method: "POST",
        headers: { "content-type": "application/json", ...sink.headers },
        body: JSON.stringify(body),
        signal: AbortSignal.timeout(10_000),
      });
      // Drain so the connection is reusable; the body is never logged.
      void response.arrayBuffer().catch(() => {});
      if (!response.ok) {
        throw new Error(`OTLP sink "${sink.name}" responded ${response.status}`);
      }
    },
  };
  return telemetry.sinks.map((sink) => makeOtelExporterConsumer(deps, sink));
}
