import { Code, ConnectError } from "@connectrpc/connect";

import {
  curateWireEvent,
  parseTerminalOutcome,
  type BoundedRead,
  type CuratedEvent,
  type TerminalOutcome,
  type WireEvent,
} from "../control-plane/session-events.ts";
import { log as rootLog } from "../log.ts";
import type { SessionConsumer } from "./consumer.ts";
import type { CursorStore } from "./cursor-store.ts";
import type { LeaseStore } from "./lease-store.ts";

const log = rootLog.child({ component: "session-listener" });
const DEFAULT_QUEUE_CAPACITY = 1_000;
const RETRY_INITIAL_MS = 250;
const RETRY_MAX_MS = 30_000;
const ERROR_EVERY_ATTEMPTS = 10;

export interface OpenedSessionStream {
  events: AsyncIterable<WireEvent>;
  close(): void;
}

export interface SessionListenerDeps {
  sessionId: string;
  owner: string;
  ttlMs: number;
  leaseStore: LeaseStore;
  cursorStore: CursorStore;
  consumers: SessionConsumer[];
  readPage(sessionId: string, after: bigint): Promise<BoundedRead>;
  openStream(sessionId: string, since: bigint): Promise<OpenedSessionStream>;
  sleep(ms: number): Promise<void>;
  queueCapacity?: number;
}

interface ConsumerState {
  consumer: SessionConsumer;
  cursor: bigint;
  highestOffered: bigint;
  queue: CuratedEvent[];
  drain: Promise<void> | null;
  spaceWaiters: Array<() => void>;
}

interface CatchUpResult {
  lastSeen: bigint;
  terminal?: TerminalOutcome;
  eventCount: number;
}

export class SessionListener {
  readonly #deps: SessionListenerDeps;
  readonly #context: { sessionId: string };
  readonly #stopped: Promise<void>;
  #resolveStopped!: () => void;
  #stopRequested = false;
  #stream: OpenedSessionStream | null = null;
  #running: Promise<void> | null = null;
  #states: ConsumerState[] = [];
  #released = false;

  constructor(deps: SessionListenerDeps) {
    this.#deps = deps;
    this.#context = { sessionId: deps.sessionId };
    this.#stopped = new Promise<void>((resolve) => {
      this.#resolveStopped = resolve;
    });
  }

  run(): Promise<void> {
    this.#running ??= this.#execute();
    return this.#running;
  }

  async stop(): Promise<void> {
    this.#requestStop();
    await this.#running;
  }

  #requestStop(): void {
    if (this.#stopRequested) return;
    this.#stopRequested = true;
    this.#resolveStopped();
    this.#stream?.close();
    for (const state of this.#states) {
      for (const wake of state.spaceWaiters.splice(0)) wake();
    }
  }

  async #execute(): Promise<void> {
    const heartbeat = this.#heartbeat();
    try {
      await this.#loadConsumers();
      await this.#listen();
    } finally {
      this.#requestStop();
      await heartbeat;
      await Promise.all(this.#states.map((state) => state.drain));
      if (!this.#released) await this.#release();
    }
  }

  async #loadConsumers(): Promise<void> {
    for (const consumer of this.#deps.consumers) {
      if (!(await consumer.appliesTo(this.#deps.sessionId))) continue;
      const cursor = await this.#deps.cursorStore.get(
        this.#deps.sessionId,
        consumer.name,
      );
      this.#states.push({
        consumer,
        cursor,
        highestOffered: cursor,
        queue: [],
        drain: null,
        spaceWaiters: [],
      });
    }
  }

  async #heartbeat(): Promise<void> {
    while (!this.#stopRequested) {
      if (!(await this.#sleepOrStop(Math.max(1, Math.floor(this.#deps.ttlMs / 3))))) {
        return;
      }
      let renewed = false;
      try {
        renewed = await this.#deps.leaseStore.renew(
          this.#deps.sessionId,
          this.#deps.owner,
          this.#deps.ttlMs,
        );
      } catch (err) {
        log.warn(
          { sessionId: this.#deps.sessionId, err },
          "listener lease renewal failed",
        );
      }
      if (!renewed) {
        log.warn(
          { sessionId: this.#deps.sessionId },
          "listener lease lost",
        );
        this.#requestStop();
        return;
      }
    }
  }

  async #listen(): Promise<void> {
    let lastSeen = this.#minimumCursor();
    let reconnectAttempt = 0;
    while (!this.#stopRequested) {
      try {
        const caughtUp = await this.#catchUp();
        if (caughtUp.lastSeen > lastSeen) lastSeen = caughtUp.lastSeen;
        log.info(
          {
            sessionId: this.#deps.sessionId,
            eventCount: caughtUp.eventCount,
            finalCursor: String(lastSeen),
          },
          "listener catch-up complete",
        );
        if (caughtUp.terminal) {
          await this.#finishTerminal(caughtUp.terminal);
          return;
        }
        if (this.#stopRequested) return;

        const opened = await this.#deps.openStream(this.#deps.sessionId, lastSeen);
        this.#stream = opened;
        log.info(
          { sessionId: this.#deps.sessionId, since: String(lastSeen) },
          "listener stream connected",
        );

        let lagged = false;
        let terminalOutcome: TerminalOutcome | undefined;
        try {
          for await (const frame of opened.events) {
            if (this.#stopRequested) return;
            if (frame.idx === undefined) {
              lagged = true;
              break;
            }
            if (frame.idx > lastSeen) lastSeen = frame.idx;
            if (frame.kind === "status_changed") {
              terminalOutcome = parseTerminalOutcome(frame.payloadJson);
              if (terminalOutcome) break;
            }
            const event = curateWireEvent(frame);
            if (event) await this.#offer(event);
          }
        } finally {
          opened.close();
          if (this.#stream === opened) this.#stream = null;
        }

        if (terminalOutcome) {
          await this.#finishTerminal(terminalOutcome);
          return;
        }
        if (this.#stopRequested) return;
        if (lagged) {
          reconnectAttempt = 0;
          continue;
        }
        throw new Error("coordinator event stream closed");
      } catch (err) {
        if (this.#stopRequested) return;
        if (err instanceof ConnectError && err.code === Code.NotFound) {
          // The coordinator no longer knows this session (GC'd / reaped): it
          // can never produce events again, so conclude instead of retrying.
          // The real outcome is unknown — consumers get no fabricated
          // onTerminal.
          log.warn(
            { sessionId: this.#deps.sessionId, err },
            "session unknown to coordinator; marking listener terminal",
          );
          await this.#deps.leaseStore.markTerminal(this.#deps.sessionId);
          await this.#release();
          this.#requestStop();
          return;
        }
        reconnectAttempt++;
        const delayMs = Math.min(
          RETRY_MAX_MS,
          RETRY_INITIAL_MS * 2 ** Math.min(reconnectAttempt - 1, 16),
        );
        log.warn(
          { sessionId: this.#deps.sessionId, err, delayMs },
          "listener stream dropped",
        );
        log.info(
          { sessionId: this.#deps.sessionId, delayMs },
          "listener stream reconnecting",
        );
        if (!(await this.#sleepOrStop(delayMs))) return;
      }
    }
  }

  async #catchUp(): Promise<CatchUpResult> {
    let after = this.#minimumCursor();
    let eventCount = 0;
    for (;;) {
      if (this.#stopRequested) return { lastSeen: after, eventCount };
      const page = await this.#deps.readPage(this.#deps.sessionId, after);
      for (const event of page.events) {
        if (this.#stopRequested) return { lastSeen: after, eventCount };
        await this.#offer(event);
        eventCount++;
      }
      const previous = after;
      after = page.nextAfter;
      if (page.terminal) {
        return { lastSeen: after, terminal: page.terminal.outcome, eventCount };
      }
      if (after === previous) return { lastSeen: after, eventCount };
    }
  }

  #minimumCursor(): bigint {
    if (this.#states.length === 0) return -1n;
    return this.#states.reduce(
      (minimum, state) => state.cursor < minimum ? state.cursor : minimum,
      this.#states[0]!.cursor,
    );
  }

  async #offer(event: CuratedEvent): Promise<void> {
    for (const state of this.#states) {
      if (event.idx <= state.highestOffered) continue;
      while (
        state.queue.length >= (this.#deps.queueCapacity ?? DEFAULT_QUEUE_CAPACITY) &&
        !this.#stopRequested
      ) {
        await Promise.race([
          new Promise<void>((resolve) => state.spaceWaiters.push(resolve)),
          this.#stopped,
        ]);
      }
      if (this.#stopRequested) return;
      state.highestOffered = event.idx;
      state.queue.push(event);
      this.#ensureDrain(state);
    }
  }

  #ensureDrain(state: ConsumerState): void {
    if (state.drain !== null || this.#stopRequested) return;
    state.drain = this.#drain(state).finally(() => {
      state.drain = null;
      if (state.queue.length > 0) this.#ensureDrain(state);
    });
  }

  async #drain(state: ConsumerState): Promise<void> {
    while (state.queue.length > 0 && !this.#stopRequested) {
      const event = state.queue.shift()!;
      for (const wake of state.spaceWaiters.splice(0)) wake();
      const completed = await this.#deliverWithRetry(state, event);
      if (!completed) return;
    }
  }

  async #deliverWithRetry(
    state: ConsumerState,
    event: CuratedEvent,
  ): Promise<boolean> {
    let attempt = 0;
    for (;;) {
      if (this.#stopRequested) return false;
      try {
        if (state.consumer.interestedIn(event.kind)) {
          await state.consumer.handle(event, this.#context);
        }
        await this.#deps.cursorStore.set(
          this.#deps.sessionId,
          state.consumer.name,
          event.idx,
        );
        state.cursor = event.idx;
        return true;
      } catch (err) {
        attempt++;
        const delayMs = Math.min(
          RETRY_MAX_MS,
          RETRY_INITIAL_MS * 2 ** Math.min(attempt - 1, 16),
        );
        const fields = {
          sessionId: this.#deps.sessionId,
          consumer: state.consumer.name,
          eventIdx: String(event.idx),
          attempt,
          delayMs,
          err,
        };
        log.warn(fields, "listener consumer handle retry");
        if (attempt % ERROR_EVERY_ATTEMPTS === 0) {
          log.error(fields, "listener consumer is still failing");
        }
        if (!(await this.#sleepOrStop(delayMs))) return false;
      }
    }
  }

  async #finishTerminal(outcome: TerminalOutcome): Promise<void> {
    await this.#waitForDrains();
    if (this.#stopRequested) return;
    await Promise.all(
      this.#states.map(async (state) => {
        if (state.consumer.onTerminal) {
          await state.consumer.onTerminal(outcome, this.#context);
        }
      }),
    );
    log.info(
      { sessionId: this.#deps.sessionId, outcome },
      "listener terminal reached",
    );
    await this.#deps.leaseStore.markTerminal(this.#deps.sessionId);
    await this.#release();
    this.#requestStop();
  }

  async #waitForDrains(): Promise<void> {
    for (;;) {
      for (const state of this.#states) {
        if (state.queue.length > 0) this.#ensureDrain(state);
      }
      const drains = this.#states.flatMap((state) =>
        state.drain === null ? [] : [state.drain]
      );
      if (drains.length === 0) return;
      await Promise.all(drains);
    }
  }

  async #release(): Promise<void> {
    await this.#deps.leaseStore.release(this.#deps.sessionId, this.#deps.owner);
    this.#released = true;
    log.info(
      { sessionId: this.#deps.sessionId },
      "listener lease released",
    );
  }

  async #sleepOrStop(ms: number): Promise<boolean> {
    const result = await Promise.race([
      this.#deps.sleep(ms).then(() => true),
      this.#stopped.then(() => false),
    ]);
    return result;
  }
}
