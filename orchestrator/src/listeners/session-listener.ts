import { Code, ConnectError } from "@connectrpc/connect";

import {
  curateWireEvent,
  parseTerminalOutcome,
  terminalOutcomeForStatus,
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
/** Deadline for one coordinator RPC (status probe, catch-up read, stream probe
 * read, recovery read). The transport can orphan a call forever — Bun's
 * node:http2 never settles a call queued on a half-open dial (issue #704) — so
 * the listener never awaits one bare. Stream open itself is synchronous (it
 * only builds the async iterable); its lazy first-pull dial — the actual park
 * risk — is bounded by the silent-stream probe and its two-sighting stall
 * verdict, not this deadline. */
const RPC_DEADLINE_MS = 15_000;
/** On a silent stream, how often to prove coordinator liveness with a bounded
 * log read (and detect a stream that is stalled behind the durable log). */
const STREAM_PROBE_INTERVAL_MS = 30_000;
/** Multiple of the lease TTL with no successful coordinator interaction after
 * which the listener abandons its lease so the scanner can start a fresh
 * replacement — possibly on another pod (issue #704: a wedged listener must
 * never keep renewing). */
const STALE_GRACE_TTL_FACTOR = 3;

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
  readPage(sessionId: string, after: bigint, signal?: AbortSignal): Promise<BoundedRead>;
  openStream(sessionId: string, since: bigint): Promise<OpenedSessionStream>;
  /** Current session status (GetSession), probed once at start: a session
   * already in a terminal status finishes immediately — its event log may
   * predate terminal status_changed events. */
  fetchStatus?(sessionId: string, signal?: AbortSignal): Promise<string>;
  /** Sleep for `ms`. If `signal` aborts first, clear the underlying timer and
   * leave the promise unsettled (the caller has already lost interest) — this
   * lets `#guarded` cancel its deadline timer the instant its RPC settles
   * early, rather than leaking a pending timer for the full deadline. */
  sleep(ms: number, signal?: AbortSignal): Promise<void>;
  queueCapacity?: number;
  /** Deadline for one coordinator RPC; default RPC_DEADLINE_MS. */
  rpcDeadlineMs?: number;
  /** Silent-stream probe cadence; default STREAM_PROBE_INTERVAL_MS. */
  probeIntervalMs?: number;
  /** No-coordinator-progress window before the listener abandons its lease;
   * default STALE_GRACE_TTL_FACTOR × ttlMs. */
  staleGraceMs?: number;
  /** Clock for staleness accounting; default Date.now. */
  now?: () => number;
}

interface ConsumerState {
  consumer: SessionConsumer;
  cursor: bigint;
  highestSeen: bigint;
  highestOffered: bigint;
  queue: CuratedEvent[];
  drain: Promise<void> | null;
  recovery: Promise<void> | null;
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
  /** `#stopped` mapped to the race sentinel, created once. `#stopped` settles
   * at most once, so a single shared reaction suffices; attaching a fresh
   * `.then` per loop iteration / per RPC would retain a closure on this
   * long-lived promise for the session's lifetime (grows with frames streamed). */
  readonly #stoppedRace: Promise<"stopped">;
  readonly #rpcDeadlineMs: number;
  readonly #probeIntervalMs: number;
  readonly #staleGraceMs: number;
  readonly #now: () => number;
  #resolveStopped!: () => void;
  #stopRequested = false;
  #stream: OpenedSessionStream | null = null;
  #running: Promise<void> | null = null;
  #states: ConsumerState[] = [];
  #released = false;
  /** Last time a coordinator interaction demonstrably succeeded (catch-up
   * page, stream frame, probe read, status probe). Drives lease liveness. */
  #lastLiveAt: number;

  constructor(deps: SessionListenerDeps) {
    this.#deps = deps;
    this.#context = { sessionId: deps.sessionId };
    this.#rpcDeadlineMs = deps.rpcDeadlineMs ?? RPC_DEADLINE_MS;
    this.#probeIntervalMs = deps.probeIntervalMs ?? STREAM_PROBE_INTERVAL_MS;
    this.#staleGraceMs = deps.staleGraceMs ?? STALE_GRACE_TTL_FACTOR * deps.ttlMs;
    this.#now = deps.now ?? Date.now;
    this.#lastLiveAt = this.#now();
    this.#stopped = new Promise<void>((resolve) => {
      this.#resolveStopped = resolve;
    });
    this.#stoppedRace = this.#stopped.then(() => "stopped" as const);
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
      await Promise.all(
        this.#states.flatMap((state) => [state.drain, state.recovery]),
      );
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
        highestSeen: cursor,
        highestOffered: cursor,
        queue: [],
        drain: null,
        recovery: null,
        spaceWaiters: [],
      });
    }
  }

  async #heartbeat(): Promise<void> {
    while (!this.#stopRequested) {
      if (!(await this.#sleepOrStop(Math.max(1, Math.floor(this.#deps.ttlMs / 3))))) {
        return;
      }
      // The lease means "I am listening", not "my process is alive" (issue
      // #704): a listener with no coordinator progress inside the grace window
      // stops renewing so the lease expires and a scanner — on any pod — can
      // start a fresh replacement, even if this listener is wedged beyond the
      // reach of #requestStop.
      const staleMs = this.#now() - this.#lastLiveAt;
      if (staleMs > this.#staleGraceMs) {
        log.error(
          { sessionId: this.#deps.sessionId, staleMs },
          "listener made no coordinator progress within the grace window; abandoning the lease for a replacement",
        );
        this.#requestStop();
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
    let statusProbed = false;
    while (!this.#stopRequested) {
      try {
        if (!statusProbed && this.#deps.fetchStatus) {
          const fetchStatus = this.#deps.fetchStatus;
          const status = await this.#guarded("status probe", (signal) =>
            fetchStatus(this.#deps.sessionId, signal));
          this.#touchLive();
          statusProbed = true;
          const outcome = terminalOutcomeForStatus(status);
          if (outcome) {
            log.info(
              { sessionId: this.#deps.sessionId, status },
              "session status is already terminal",
            );
            await this.#finishTerminal(outcome);
            return;
          }
        }
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

        // No #guarded here: openStream is synchronous — it only constructs the
        // async iterable. The dial happens lazily on the first iterator.next()
        // below, where the probe timer + stall verdict bound it (issue #704);
        // a deadline around this synchronous call would never fire and its
        // abort would reach nothing.
        const opened = await this.#deps.openStream(this.#deps.sessionId, lastSeen);
        this.#stream = opened;
        log.info(
          { sessionId: this.#deps.sessionId, since: String(lastSeen) },
          "listener stream connected",
        );

        let lagged = false;
        let terminalOutcome: TerminalOutcome | undefined;
        // Pull frames manually so a silent stream can be probed: the dial is
        // lazy (it happens on the first pull) and can park forever (issue
        // #704), which is indistinguishable from a healthy idle session
        // without a periodic liveness read against the durable log.
        const iterator = opened.events[Symbol.asyncIterator]();
        let pending: Promise<IteratorResult<WireEvent, unknown>> | null = null;
        // One probe timer, re-armed only when it actually fires — never torn
        // down and rebuilt per frame (that would allocate a fresh uncancellable
        // timer on the hot streaming path for every wire frame). A delivered
        // frame just sets frameSinceProbe; when the timer fires we consult that
        // flag: a stream that delivered this interval is demonstrably live
        // (#touchLive already ran), so we skip the probe read and start a fresh
        // interval. Only a silent stream reaches the read, and a stalled one
        // reads on two consecutive firings with no frame between (sawLogAhead
        // persists) — which condemns it. Idle sessions (no frames, log at
        // cursor) still probe every interval, which is what keeps their lease
        // alive.
        let probeTimer: Promise<"probe"> | null = null;
        let sawLogAhead = false;
        let frameSinceProbe = false;
        try {
          for (;;) {
            pending ??= iterator.next();
            probeTimer ??= this.#deps.sleep(this.#probeIntervalMs).then(() => "probe" as const);
            const winner = await Promise.race([
              pending.then(
                (result) => ({ result }),
                (err: unknown) => ({ err }),
              ),
              probeTimer,
              this.#stoppedRace,
            ]);
            if (winner === "stopped") return;
            if (winner === "probe") {
              probeTimer = null; // fired: the next iteration re-arms it
              if (frameSinceProbe) {
                frameSinceProbe = false;
                continue; // delivered this interval — no probe read needed
              }
              sawLogAhead = await this.#probeQuietStream(lastSeen, sawLogAhead);
              continue;
            }
            if ("err" in winner) throw winner.err;
            pending = null;
            if (winner.result.done) break;
            const frame = winner.result.value;
            if (this.#stopRequested) return;
            this.#touchLive();
            sawLogAhead = false;
            frameSinceProbe = true; // re-arm decision deferred to the next firing
            reconnectAttempt = 0;
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
          try {
            void Promise.resolve(iterator.return?.()).catch(() => {});
          } catch {
            // best-effort iterator teardown; opened.close() is authoritative
          }
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
        const staleMs = this.#now() - this.#lastLiveAt;
        if (staleMs > this.#staleGraceMs) {
          // Retrying on this pod has proven fruitless for a whole grace
          // window; exit cleanly (releasing the lease) so a scanner starts a
          // fresh listener — with fresh transport state, possibly elsewhere.
          log.error(
            { sessionId: this.#deps.sessionId, staleMs, err },
            "listener made no coordinator progress within the grace window; exiting for a replacement",
          );
          return;
        }
        reconnectAttempt++;
        const delayMs = Math.min(
          RETRY_MAX_MS,
          RETRY_INITIAL_MS * 2 ** Math.min(reconnectAttempt - 1, 16),
        );
        log.warn(
          { sessionId: this.#deps.sessionId, err, delayMs, attempt: reconnectAttempt },
          "listener stream dropped",
        );
        log.info(
          { sessionId: this.#deps.sessionId, delayMs, attempt: reconnectAttempt },
          "listener stream reconnecting",
        );
        if (!(await this.#sleepOrStop(delayMs))) return;
      }
    }
  }

  #touchLive(): void {
    this.#lastLiveAt = this.#now();
  }

  /** Run one coordinator RPC with a deadline. The transport can orphan a call
   * forever (issue #704: Bun's node:http2 never settles a call queued on a
   * half-open dial), so no coordinator await may run bare: on deadline the
   * call is aborted and the failure takes the ordinary retry path. */
  async #guarded<T>(
    what: string,
    start: (signal: AbortSignal) => Promise<T>,
  ): Promise<T> {
    const controller = new AbortController();
    // Separate signal for the deadline timer: aborting it clears the timer
    // (via the sleep seam) the moment the RPC settles, so a fast read does not
    // leave a full-deadline timer pending — bursty during start-of-session
    // catch-up otherwise.
    const deadline = new AbortController();
    const work = start(controller.signal);
    try {
      const outcome = await Promise.race([
        work.then(
          (value) => ({ ok: true as const, value }),
          (err: unknown) => ({ ok: false as const, err }),
        ),
        this.#deps.sleep(this.#rpcDeadlineMs, deadline.signal).then(() => "deadline" as const),
        this.#stoppedRace,
      ]);
      if (typeof outcome === "string") {
        controller.abort();
        void work.catch(() => {});
        throw new Error(
          outcome === "deadline"
            ? `coordinator rpc exceeded its ${this.#rpcDeadlineMs}ms deadline: ${what}`
            : "listener stopped",
        );
      }
      if (!outcome.ok) throw outcome.err;
      return outcome.value;
    } finally {
      deadline.abort();
    }
  }

  /** The stream produced nothing for a whole probe interval. One bounded read
   * of the durable log distinguishes the three possibilities: a read failure
   * (coordinator unreachable — staleness accrues toward the grace window), a
   * page at the stream cursor (genuinely idle — proves liveness), or a page
   * beyond it. The stream gets one full interval to deliver before a second
   * consecutive ahead-sighting declares it stalled and forces a reconnect. */
  async #probeQuietStream(lastSeen: bigint, sawLogAhead: boolean): Promise<boolean> {
    let page: BoundedRead;
    try {
      page = await this.#guarded("stream probe read", (signal) =>
        this.#deps.readPage(this.#deps.sessionId, lastSeen, signal));
    } catch (err) {
      if (this.#stopRequested || this.#now() - this.#lastLiveAt > this.#staleGraceMs) {
        throw err;
      }
      log.warn(
        { sessionId: this.#deps.sessionId, err },
        "listener stream probe failed",
      );
      return sawLogAhead;
    }
    this.#touchLive();
    if (page.nextAfter <= lastSeen) return false;
    if (!sawLogAhead) return true;
    throw new Error("event stream is stalled behind the durable log");
  }

  async #catchUp(): Promise<CatchUpResult> {
    let after = this.#minimumCursor();
    let eventCount = 0;
    for (;;) {
      if (this.#stopRequested) return { lastSeen: after, eventCount };
      const page = await this.#guarded("catch-up read", (signal) =>
        this.#deps.readPage(this.#deps.sessionId, after, signal));
      this.#touchLive();
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
      if (event.idx > state.highestSeen) state.highestSeen = event.idx;
      if (event.idx <= state.highestOffered) continue;
      if (
        state.recovery !== null ||
        state.queue.length >= (this.#deps.queueCapacity ?? DEFAULT_QUEUE_CAPACITY)
      ) {
        // Never hold the shared stream behind one full consumer. Its durable
        // cursor lets an independent recovery task replay the skipped suffix.
        this.#ensureRecovery(state);
        continue;
      }
      this.#enqueue(state, event);
    }
  }

  #enqueue(state: ConsumerState, event: CuratedEvent): void {
    if (event.idx <= state.highestOffered) return;
    state.highestOffered = event.idx;
    state.queue.push(event);
    this.#ensureDrain(state);
  }

  #ensureRecovery(state: ConsumerState): void {
    if (state.recovery !== null || this.#stopRequested) return;
    state.recovery = this.#recover(state).finally(() => {
      state.recovery = null;
      // An event can extend highestSeen while the previous recovery is
      // completing. Re-check after clearing the in-flight marker.
      if (state.highestOffered < state.highestSeen) this.#ensureRecovery(state);
    });
  }

  async #recover(state: ConsumerState): Promise<void> {
    let after = state.highestOffered;
    let attempt = 0;
    while (!this.#stopRequested && after < state.highestSeen) {
      try {
        const page = await this.#guarded("recovery read", (signal) =>
          this.#deps.readPage(this.#deps.sessionId, after, signal));
        this.#touchLive();
        attempt = 0;
        for (const event of page.events) {
          if (event.idx <= state.highestOffered) continue;
          if (event.idx > state.highestSeen) state.highestSeen = event.idx;
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
          this.#enqueue(state, event);
        }
        if (page.nextAfter <= after) {
          if (!(await this.#sleepOrStop(RETRY_INITIAL_MS))) return;
        } else {
          after = page.nextAfter;
        }
      } catch (err) {
        attempt++;
        const delayMs = Math.min(
          RETRY_MAX_MS,
          RETRY_INITIAL_MS * 2 ** Math.min(attempt - 1, 16),
        );
        log.warn(
          {
            sessionId: this.#deps.sessionId,
            consumer: state.consumer.name,
            after: String(after),
            delayMs,
            err,
          },
          "listener consumer catch-up retry",
        );
        if (!(await this.#sleepOrStop(delayMs))) return;
      }
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
        if (state.highestOffered < state.highestSeen) this.#ensureRecovery(state);
      }
      const work = this.#states.flatMap((state) =>
        [state.drain, state.recovery].filter(
          (pending): pending is Promise<void> => pending !== null,
        )
      );
      if (work.length === 0) return;
      await Promise.all(work);
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
