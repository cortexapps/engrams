import { describe, expect, test } from "bun:test";
import pino from "pino";

import {
  draftingSeedPromptId,
  runDraftingSeedTick,
  type DraftingSeedSender,
  type DraftingSeedWork,
  type SpecDraftingSeedStore,
} from "../drafting-seed-scanner.ts";

const SPEC_ID = "00000000-0000-4000-8000-000000001160";
const SESSION_ID = "00000000-0000-4000-8000-000000001161";
const STARTED_AT = new Date("2026-08-13T20:00:00.000Z");
const RETRY_DELAY_MS = 15_000;
const log = pino({ enabled: false });

class MemoryDraftingSeedStore implements SpecDraftingSeedStore {
  readonly phase = "drafting";
  state: "pending" | "delivered" = "pending";
  attempts = 0;
  nextAttemptAt = STARTED_AT;
  deliveredAt: Date | null = null;
  lastError: string | null = null;
  readonly work: DraftingSeedWork = {
    specId: SPEC_ID,
    sessionId: SESSION_ID,
    promptId: draftingSeedPromptId(SPEC_ID),
    text: "[start drafting — requested by Ada]",
    attempts: 0,
  };

  async start(): Promise<never> {
    throw new Error("the test starts after the phase and seed intent commit");
  }

  async claimDue(input: {
    now: Date;
    retryAt: Date;
    limit: number;
    specId?: string;
  }): Promise<DraftingSeedWork[]> {
    if (
      input.limit === 0 ||
      this.state !== "pending" ||
      this.nextAttemptAt > input.now ||
      (input.specId !== undefined && input.specId !== SPEC_ID)
    ) {
      return [];
    }
    this.attempts += 1;
    this.nextAttemptAt = input.retryAt;
    this.lastError = null;
    return [{ ...this.work, attempts: this.attempts }];
  }

  async markDelivered(input: { at: Date }): Promise<boolean> {
    if (this.state !== "pending") return false;
    this.state = "delivered";
    this.deliveredAt = input.at;
    this.lastError = null;
    return true;
  }

  async recordFailure(input: { error: string; retryAt: Date }): Promise<void> {
    if (this.state !== "pending") return;
    this.lastError = input.error;
    this.nextAttemptAt = input.retryAt;
  }
}

describe("drafting seed scanner", () => {
  test("a fault after SendPrompt accepts the committed seed still delivers it exactly once", async () => {
    const store = new MemoryDraftingSeedStore();
    const accepted = new Map<string, DraftingSeedWork>();
    let sendCalls = 0;
    let clock = STARTED_AT.getTime();
    const sender: DraftingSeedSender = {
      async send(work) {
        sendCalls += 1;
        // This models the coordinator accepting the prompt before the caller
        // loses the response. Its real outbox has the same prompt-id dedupe.
        if (!accepted.has(work.promptId)) accepted.set(work.promptId, work);
        if (sendCalls === 1) throw new Error("connection reset after prompt accept");
      },
    };
    const deps = {
      store,
      sender,
      config: { batchSize: 20, retryDelayMs: RETRY_DELAY_MS },
      now: () => new Date(clock),
      log,
    };

    expect(await runDraftingSeedTick(deps)).toEqual({ claimed: 1, delivered: 0, failed: 1 });
    expect(store.phase).toBe("drafting");
    expect(store.state).toBe("pending");
    expect(accepted.size).toBe(1);

    clock += RETRY_DELAY_MS;
    expect(await runDraftingSeedTick(deps)).toEqual({ claimed: 1, delivered: 1, failed: 0 });
    expect(store.state).toBe("delivered");
    expect(sendCalls).toBe(2);
    expect([...accepted.keys()]).toEqual([draftingSeedPromptId(SPEC_ID)]);

    clock += RETRY_DELAY_MS;
    expect(await runDraftingSeedTick(deps)).toEqual({ claimed: 0, delivered: 0, failed: 0 });
    expect(accepted.size).toBe(1);
  });
});
