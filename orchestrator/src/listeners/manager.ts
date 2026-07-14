import { log as rootLog } from "../log.ts";
import { hostname } from "node:os";
import { sessions } from "../control-plane/client.ts";
import { readSessionEventsBounded } from "../control-plane/session-events.ts";
import { makeCursorStore } from "./cursor-store.ts";
import { makeLeaseStore } from "./lease-store.ts";
import type { LeaseStore } from "./lease-store.ts";
import { SessionListener } from "./session-listener.ts";
import { makeProductionSlackConsumer } from "./slack-consumer.ts";
import { makeProductionToolConsumer } from "./tool-consumer.ts";

const log = rootLog.child({ component: "listener-manager" });
const SCAN_INTERVAL_MS = 5_000;

export interface ListenerHandle {
  start(): Promise<void>;
  stop(): Promise<void>;
}

export interface ListenerManagerDeps {
  owner: string;
  ttlMs: number;
  leaseStore: LeaseStore;
  createListener(sessionId: string): ListenerHandle;
  setInterval?: (fn: () => void, ms: number) => ReturnType<typeof setInterval>;
  clearInterval?: (timer: ReturnType<typeof setInterval>) => void;
}

export class ListenerManager {
  readonly #deps: ListenerManagerDeps;
  readonly #listeners = new Map<string, ListenerHandle>();
  #timer: ReturnType<typeof setInterval> | null = null;
  #scan: Promise<void> | null = null;

  constructor(deps: ListenerManagerDeps) {
    this.#deps = deps;
  }

  scanOnce(): Promise<void> {
    this.#scan ??= this.#scanImpl().finally(() => {
      this.#scan = null;
    });
    return this.#scan;
  }

  async #scanImpl(): Promise<void> {
    const desired = new Set(await this.#deps.leaseStore.listDesired());

    for (const [sessionId, listener] of [...this.#listeners]) {
      if (desired.has(sessionId)) continue;
      this.#listeners.delete(sessionId);
      try {
        await listener.stop();
      } catch (err) {
        log.error({ sessionId, err }, "listener scanner failed to stop a listener");
      } finally {
        await this.#releaseLease(sessionId);
        log.info({ sessionId }, "listener scanner stopped a listener");
      }
    }

    for (const sessionId of desired) {
      if (this.#listeners.has(sessionId)) continue;
      let acquired = false;
      try {
        acquired = await this.#deps.leaseStore.tryAcquire(
          sessionId,
          this.#deps.owner,
          this.#deps.ttlMs,
        );
        if (!acquired) continue;
        log.info({ sessionId }, "listener lease acquired");
        const listener = this.#deps.createListener(sessionId);
        this.#listeners.set(sessionId, listener);
        const running = listener.start();
        void running.then(
          () => {
            if (this.#listeners.get(sessionId) === listener) {
              this.#listeners.delete(sessionId);
            }
          },
          async (err) => {
            log.error({ sessionId, err }, "session listener failed");
            if (this.#listeners.get(sessionId) === listener) {
              this.#listeners.delete(sessionId);
            }
            await this.#releaseLease(sessionId);
          },
        );
        log.info({ sessionId }, "listener scanner started a listener");
      } catch (err) {
        log.error({ sessionId, err }, "listener scanner failed to start a listener");
        if (acquired) {
          await this.#releaseLease(sessionId);
        }
      }
    }
  }

  async start(): Promise<void> {
    if (this.#timer !== null) return;
    await this.scanOnce();
    const schedule = this.#deps.setInterval ?? setInterval;
    this.#timer = schedule(() => void this.scanOnce(), SCAN_INTERVAL_MS);
  }

  async stop(): Promise<void> {
    if (this.#timer !== null) {
      const cancel = this.#deps.clearInterval ?? clearInterval;
      cancel(this.#timer);
      this.#timer = null;
    }
    await this.#scan;
    for (const [sessionId, listener] of [...this.#listeners]) {
      this.#listeners.delete(sessionId);
      try {
        await listener.stop();
      } catch (err) {
        log.error({ sessionId, err }, "listener scanner failed to stop a listener");
      } finally {
        await this.#releaseLease(sessionId);
        log.info({ sessionId }, "listener scanner stopped a listener");
      }
    }
  }

  async #releaseLease(sessionId: string): Promise<void> {
    try {
      await this.#deps.leaseStore.release(sessionId, this.#deps.owner);
      log.info({ sessionId }, "listener lease released");
    } catch (err) {
      log.error({ sessionId, err }, "listener scanner failed to release a lease");
    }
  }
}

const PRODUCTION_LEASE_TTL_MS = 30_000;

/** Build the process singleton with coordinator stream/catch-up and both
 * production consumers wired for every acquired session. */
export function makeProductionListenerManager(): ListenerManager {
  const owner = `${hostname()}:${process.pid}:${crypto.randomUUID()}`;
  const leaseStore = makeLeaseStore();
  const cursorStore = makeCursorStore();
  return new ListenerManager({
    owner,
    ttlMs: PRODUCTION_LEASE_TTL_MS,
    leaseStore,
    createListener(sessionId) {
      const listener = new SessionListener({
        sessionId,
        owner,
        ttlMs: PRODUCTION_LEASE_TTL_MS,
        leaseStore,
        cursorStore,
        consumers: [
          makeProductionToolConsumer(),
          makeProductionSlackConsumer(),
        ],
        readPage: readSessionEventsBounded,
        openStream: async (id, since) => {
          const abort = new AbortController();
          const events = sessions.streamEvents(
            {
              sessionId: id,
              ...(since >= 0n ? { since } : {}),
            },
            { signal: abort.signal },
          );
          return {
            events,
            close: () => abort.abort(),
          };
        },
        sleep: (ms) => Bun.sleep(ms),
      });
      return {
        start: () => listener.run(),
        stop: () => listener.stop(),
      };
    },
  });
}
