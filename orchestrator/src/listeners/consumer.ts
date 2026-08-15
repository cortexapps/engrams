import type {
  CuratedEvent,
  TerminalOutcome,
} from "../control-plane/session-events.ts";

export interface SessionConsumerContext {
  sessionId: string;
}

export interface SessionConsumer {
  name: string;
  /**
   * A RAW consumer receives every durable (idx-bearing) event, uncurated —
   * including kinds `CURATED_KINDS` drops (tool_call_started, generation,
   * status_changed, stdout, …). Absent/false = today's curated stream,
   * byte-identical to before the flag existed.
   */
  raw?: true;
  interestedIn(kind: string): boolean;
  appliesTo(sessionId: string): Promise<boolean>;
  /**
   * Handle one event. Returning a bigint overrides the cursor commit for
   * this delivery: a consumer that buffers events across deliveries (the
   * otel exporter's open turn) returns its held floor — the idx just before
   * the earliest event it still needs replayed after a restart — and returns
   * `event.idx` once the buffer is flushed. A void return keeps the default
   * (commit `event.idx`).
   */
  handle(event: CuratedEvent, ctx: SessionConsumerContext): Promise<void | bigint>;
  onTerminal?(
    outcome: TerminalOutcome,
    ctx: SessionConsumerContext,
  ): Promise<void>;
}
