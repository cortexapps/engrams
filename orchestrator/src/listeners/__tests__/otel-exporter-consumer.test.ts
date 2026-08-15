/**
 * otel-exporter consumer: buffering, the commit-idx protocol, the retry
 * budget, rewind/epoch handling, and the terminal flush — all against an
 * injected post fn (no network, no DB).
 */

import { describe, expect, test } from "bun:test";

import type { TelemetrySink } from "../../config.ts";
import type { ExportTraceServiceRequest } from "../../telemetry/otlp.ts";
import type { SessionIdentity } from "../../telemetry/span-builder.ts";
import { makeOtelExporterConsumer, type OtelExporterDeps } from "../otel-exporter-consumer.ts";

const SESSION = "11111111-2222-3333-4444-555555555555";
const CTX = { sessionId: SESSION };

const IDENTITY: SessionIdentity = {
  taskId: "task-1",
  rootTaskId: "root-1",
  createdByUserId: "user-1",
  harness: "claude",
  modelRouter: null,
  profileId: null,
  taskCreatedAtMs: Date.parse("2026-08-15T10:00:00Z"),
};

function sink(overrides: Partial<TelemetrySink> = {}): TelemetrySink {
  return {
    name: "langfuse",
    endpoint: "https://langfuse.example/api/public/otel/v1/traces",
    headers: {},
    captureContent: false,
    serviceName: "engrams",
    ...overrides,
  };
}

interface Harness {
  consumer: ReturnType<typeof makeOtelExporterConsumer>;
  posted: ExportTraceServiceRequest[];
  failPosts: { value: boolean };
}

function makeHarness(sinkOverrides: Partial<TelemetrySink> = {}): Harness {
  const posted: ExportTraceServiceRequest[] = [];
  const failPosts = { value: false };
  const deps: OtelExporterDeps = {
    lookupSessionIdentity: () => Promise.resolve(IDENTITY),
    post: (_sink, body) => {
      if (failPosts.value) return Promise.reject(new Error("sink down"));
      posted.push(body);
      return Promise.resolve();
    },
    nowMs: () => Date.parse("2026-08-15T12:00:00Z"),
  };
  return { consumer: makeOtelExporterConsumer(deps, sink(sinkOverrides)), posted, failPosts };
}

function ev(idx: bigint, kind: string, payload: Record<string, unknown>) {
  return { idx, kind, payloadJson: JSON.stringify(payload) };
}

const AT = "2026-08-15T11:00:00Z";

function spansOf(body: ExportTraceServiceRequest) {
  return body.resourceSpans[0]!.scopeSpans[0]!.spans;
}

describe("otel-exporter consumer", () => {
  test("buffers a run, holds the commit floor, posts once at run end", async () => {
    const { consumer, posted } = makeHarness();
    expect(await consumer.appliesTo(SESSION)).toBe(true);

    // While the run is open every delivery commits run_started.idx - 1.
    expect(
      await consumer.handle(ev(10n, "run_started", { run_id: "r1", prompt_id: "p1", at: AT }), CTX),
    ).toBe(9n);
    expect(
      await consumer.handle(
        ev(11n, "generation", {
          run_id: "r1",
          message_id: "m1",
          model: "claude-sonnet-5",
          input_tokens: 1,
          output_tokens: 2,
          cache_read_tokens: 0,
          cache_creation_tokens: 0,
          at: AT,
        }),
        CTX,
      ),
    ).toBe(9n);
    // Irrelevant kinds no-op but still hold the floor.
    expect(await consumer.handle(ev(12n, "stdout", { bytes_start: 0 }), CTX)).toBe(9n);
    expect(posted).toHaveLength(0);

    // Run end: one POST, floor released, commit = event.idx.
    expect(
      await consumer.handle(ev(13n, "run_completed", { run_id: "r1", ok: true, at: AT }), CTX),
    ).toBe(13n);
    expect(posted).toHaveLength(1);
    const spans = spansOf(posted[0]!);
    expect(spans.map((s) => s.name)).toEqual(["chat claude-sonnet-5", "agent_turn"]);
  });

  test("transient post failures rethrow (pump retries), then drop after the budget", async () => {
    const { consumer, posted, failPosts } = makeHarness();
    await consumer.handle(ev(1n, "run_started", { run_id: "r1", at: AT }), CTX);
    failPosts.value = true;
    const done = ev(2n, "run_completed", { run_id: "r1", ok: true, at: AT });
    for (let i = 0; i < 7; i++) {
      await expect(consumer.handle(done, CTX)).rejects.toThrow("sink down");
    }
    // 8th attempt: budget spent — drop the flush and advance.
    expect(await consumer.handle(done, CTX)).toBe(2n);
    expect(posted).toHaveLength(0);
    // The run is released: later events commit at their own idx.
    expect(await consumer.handle(ev(3n, "harness_idle", { at: AT }), CTX)).toBe(3n);
  });

  test("rewound (tombstoned) events are skipped entirely", async () => {
    const { consumer, posted } = makeHarness();
    await consumer.handle(
      ev(1n, "run_started", { run_id: "r1", at: AT, _rewound: true }),
      CTX,
    );
    // The rewound run never buffered, so nothing holds the floor.
    expect(await consumer.handle(ev(2n, "harness_idle", { at: AT }), CTX)).toBe(2n);
    expect(posted).toHaveLength(0);
  });

  test("epoch: replayed epochs and recovered_from_checkpoint change span ids, not the trace", async () => {
    const runAt = async (harness: Harness, epochEvents: () => Promise<void>) => {
      await epochEvents();
      await harness.consumer.handle(ev(10n, "run_started", { run_id: "r1", at: AT }), CTX);
      await harness.consumer.handle(
        ev(11n, "run_completed", { run_id: "r1", ok: true, at: AT }),
        CTX,
      );
      return spansOf(harness.posted.at(-1)!);
    };
    const plain = makeHarness();
    const spansEpoch0 = await runAt(plain, async () => {});
    const bumped = makeHarness();
    const spansEpoch2 = await runAt(bumped, async () => {
      await bumped.consumer.handle(
        ev(5n, "recovered_from_checkpoint", { recovery_epoch: 2, at: AT }),
        CTX,
      );
    });
    expect(spansEpoch2[0]!.traceId).toBe(spansEpoch0[0]!.traceId);
    expect(spansEpoch2.map((s) => s.spanId)).not.toContain(spansEpoch0.at(-1)!.spanId);
  });

  test("onTerminal flushes open runs and the session span; failures are swallowed", async () => {
    const { consumer, posted, failPosts } = makeHarness();
    await consumer.handle(ev(1n, "run_started", { run_id: "r1", at: AT }), CTX);
    await consumer.onTerminal!("completed", CTX);
    expect(posted).toHaveLength(1);
    const names = spansOf(posted[0]!).map((s) => s.name);
    expect(names).toContain("agent_turn"); // the open run, flushed as interrupted
    expect(names).toContain("agent_session");
    const turn = spansOf(posted[0]!).find((s) => s.name === "agent_turn")!;
    expect(JSON.stringify(turn.attributes)).toContain("engrams.interrupted");

    // A dead sink at terminal must not throw into the listener.
    const failing = makeHarness();
    failing.failPosts.value = true;
    await failing.consumer.handle(ev(1n, "run_started", { run_id: "r1", at: AT }), CTX);
    await expect(failing.consumer.onTerminal!("failed", CTX)).resolves.toBeUndefined();
  });

  test("early flush bounds a marathon turn without advancing the floor", async () => {
    const { consumer, posted } = makeHarness();
    await consumer.handle(ev(1n, "run_started", { run_id: "r1", at: AT }), CTX);
    // One oversized tool result blows the 256 KiB buffer budget.
    const big = "x".repeat(300 * 1024);
    const commit = await consumer.handle(
      ev(2n, "tool_call_completed", {
        run_id: "r1",
        tool_call_id: "t1",
        tool_name: "Bash",
        ok: true,
        duration_ms: 5,
        result_summary: big,
        at: AT,
      }),
      CTX,
    );
    expect(posted).toHaveLength(1); // children + provisional turn posted now
    expect(commit).toBe(0n); // floor still held at run_started - 1
    // Final flush still emits the (complete) turn span under the same id.
    await consumer.handle(ev(3n, "run_completed", { run_id: "r1", ok: true, at: AT }), CTX);
    expect(posted).toHaveLength(2);
    const provisionalTurn = spansOf(posted[0]!).find((s) => s.name === "agent_turn")!;
    const finalTurn = spansOf(posted[1]!).find((s) => s.name === "agent_turn")!;
    expect(finalTurn.spanId).toBe(provisionalTurn.spanId); // upsert, not duplicate
  });

  test("multi-sink isolation: one failing sink never blocks the other's commits", async () => {
    const healthy = makeHarness({ name: "a" });
    const failing = makeHarness({ name: "b" });
    failing.failPosts.value = true;
    expect(healthy.consumer.name).toBe("otel-exporter:a");
    expect(failing.consumer.name).toBe("otel-exporter:b");

    const started = ev(1n, "run_started", { run_id: "r1", at: AT });
    const done = ev(2n, "run_completed", { run_id: "r1", ok: true, at: AT });
    await healthy.consumer.handle(started, CTX);
    await failing.consumer.handle(started, CTX);
    expect(await healthy.consumer.handle(done, CTX)).toBe(2n); // advances
    await expect(failing.consumer.handle(done, CTX)).rejects.toThrow(); // retries alone
    expect(healthy.posted).toHaveLength(1);
  });

  test("per-sink content gating", async () => {
    const capture = makeHarness({ captureContent: true });
    const metadata = makeHarness();
    const events = [
      ev(1n, "run_started", { run_id: "r1", prompt_summary: "secret prompt", at: AT }),
      ev(2n, "run_completed", { run_id: "r1", ok: true, at: AT }),
    ];
    for (const e of events) await capture.consumer.handle(e, CTX);
    for (const e of events) await metadata.consumer.handle(e, CTX);
    expect(JSON.stringify(capture.posted[0])).toContain("secret prompt");
    expect(JSON.stringify(metadata.posted[0])).not.toContain("secret prompt");
  });
});
