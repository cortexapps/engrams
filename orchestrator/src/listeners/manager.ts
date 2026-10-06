import { Code, ConnectError } from "@connectrpc/connect";

import { log as rootLog } from "../log.ts";
import { hostname } from "node:os";
import { sessions } from "../control-plane/client.ts";
import { PARKED_STATUS, readSessionEventsBounded } from "../control-plane/session-events.ts";
import { makeCursorStore } from "./cursor-store.ts";
import { makeLeaseStore } from "./lease-store.ts";
import type { LeaseStore } from "./lease-store.ts";
import { SessionListener } from "./session-listener.ts";
import { makeProductionOtelExporterConsumers } from "./otel-exporter-consumer.ts";
import { makeProductionPrLinkConsumer } from "./pr-link-consumer.ts";
import { makeProductionAutomationConsumer } from "./automation-consumer.ts";
import { makeProductionTitleConsumer } from "./title-consumer.ts";
import { makeProductionToolConsumer } from "./tool-consumer.ts";
import { config } from "../config.ts";
import { makeSpecProjectionConsumer } from "./spec-projection-consumer.ts";
import { productionSpecProjection } from "../specs/projection.ts";

const log = rootLog.child({ component: "listener-manager" });
const SCAN_INTERVAL_MS = 5_000;
/** How often dormant rows are checked against the coordinator. Every resume
 * is an orchestrator RPC and wakes its row on the spot; this reconcile is
 * the safety net for a path that did not (an operator acting on the
 * coordinator directly), and it retires the rows of sessions that are gone. */
export const DORMANT_RECONCILE_INTERVAL_MS = 10 * 60_000;
const PROBE_DEADLINE_MS = 15_000;

export interface ListenerHandle {
  start(): Promise<void>;
  stop(): Promise<void>;
}

export interface ListenerManagerDeps {
  owner: string;
  ttlMs: number;
  leaseStore: LeaseStore;
  createListener(sessionId: string): ListenerHandle;
  /** Current coordinator status of a session (GetSession); throws a NotFound
   * ConnectError for a session the coordinator no longer knows. Drives the
   * dormant reconcile; absent = no reconcile. */
  probeStatus?: (sessionId: string) => Promise<string>;
  setInterval?: (fn: () => void, ms: number) => ReturnType<typeof setInterval>;
  clearInterval?: (timer: ReturnType<typeof setInterval>) => void;
}

export class ListenerManager {
  readonly #deps: ListenerManagerDeps;
  readonly #listeners = new Map<string, ListenerHandle>();
  #timer: ReturnType<typeof setInterval> | null = null;
  #reconcileTimer: ReturnType<typeof setInterval> | null = null;
  #scan: Promise<void> | null = null;
  #reconcile: Promise<void> | null = null;

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

  /** Check every dormant row against the coordinator once: a session that
   * is no longer parked is woken (its listener drains the log, and finishes
   * a terminal session properly); one the coordinator no longer knows is
   * marked terminal. A parked one stays dormant. */
  reconcileDormantOnce(): Promise<void> {
    this.#reconcile ??= this.#reconcileImpl().finally(() => {
      this.#reconcile = null;
    });
    return this.#reconcile;
  }

  async #reconcileImpl(): Promise<void> {
    const probe = this.#deps.probeStatus;
    if (!probe) return;
    for (const sessionId of await this.#deps.leaseStore.listDormant()) {
      try {
        const status = await probe(sessionId);
        if (status === PARKED_STATUS) continue;
        await this.#deps.leaseStore.wake(sessionId);
        log.info({ sessionId, status }, "dormant listener woken: the session is no longer parked");
      } catch (err) {
        if (err instanceof ConnectError && err.code === Code.NotFound) {
          await this.#deps.leaseStore.markTerminal(sessionId);
          log.info({ sessionId }, "dormant listener retired: the session is gone");
          continue;
        }
        log.warn({ sessionId, err }, "dormant listener reconcile probe failed");
      }
    }
  }

  async start(): Promise<void> {
    if (this.#timer !== null) return;
    await this.scanOnce();
    const schedule = this.#deps.setInterval ?? setInterval;
    this.#timer = schedule(() => void this.scanOnce(), SCAN_INTERVAL_MS);
    if (this.#deps.probeStatus) {
      this.#reconcileTimer = schedule(
        () => void this.reconcileDormantOnce(),
        DORMANT_RECONCILE_INTERVAL_MS,
      );
    }
  }

  async stop(): Promise<void> {
    const cancel = this.#deps.clearInterval ?? clearInterval;
    if (this.#timer !== null) {
      cancel(this.#timer);
      this.#timer = null;
    }
    if (this.#reconcileTimer !== null) {
      cancel(this.#reconcileTimer);
      this.#reconcileTimer = null;
    }
    await this.#scan;
    await this.#reconcile;
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

/** Build the process singleton with coordinator stream/catch-up and all
 * production consumers wired for every acquired session. */
export function makeProductionListenerManager(): ListenerManager {
  const owner = `${hostname()}:${process.pid}:${crypto.randomUUID()}`;
  const leaseStore = makeLeaseStore();
  const cursorStore = makeCursorStore();
  return new ListenerManager({
    owner,
    ttlMs: PRODUCTION_LEASE_TTL_MS,
    leaseStore,
    probeStatus: async (sessionId) => {
      const response = await sessions.getSession(
        { sessionId },
        { signal: AbortSignal.timeout(PROBE_DEADLINE_MS) },
      );
      return response.session?.status ?? "";
    },
    createListener(sessionId) {
      const listener = new SessionListener({
        sessionId,
        owner,
        ttlMs: PRODUCTION_LEASE_TTL_MS,
        leaseStore,
        cursorStore,
        consumers: [
          makeProductionToolConsumer(),
          makeProductionPrLinkConsumer(),
          makeProductionAutomationConsumer(),
          makeProductionTitleConsumer(),
          makeSpecProjectionConsumer(productionSpecProjection),
          // [] when config.telemetry is unset — telemetry off costs nothing.
          ...makeProductionOtelExporterConsumers(config.telemetry),
        ],
        readPage: (id, after, signal) =>
          readSessionEventsBounded(id, after, undefined, signal),
        fetchStatus: async (id, signal) => {
          const response = await sessions.getSession(
            { sessionId: id },
            signal ? { signal } : {},
          );
          return response.session?.status ?? "";
        },
        openStream: async (id, since) => {
          const abort = new AbortController();
          const events = sessions.streamEvents(
            {
              sessionId: id,
              ...(since >= 0n ? { since } : {}),
              // ADR 0108 B: this consumer is cursor-based; ephemeral chunk
              // frames carry no idx and are suppressed server-side. Old
              // coordinators ignore the flag — the listener's lag test
              // must therefore still tolerate idx-less chunk frames.
              durableOnly: true,
            },
            { signal: abort.signal },
          );
          return {
            events,
            close: () => abort.abort(),
          };
        },
        sleep: (ms, signal) => {
          if (!signal) return Bun.sleep(ms);
          if (signal.aborted) return Promise.resolve();
          return new Promise<void>((resolve) => {
            const onAbort = () => {
              clearTimeout(timer);
              resolve();
            };
            // Remove the abort listener when the timer wins, so a recurring
            // caller on a long-lived signal (e.g. the heartbeat's stop signal)
            // doesn't accumulate listeners.
            const timer = setTimeout(() => {
              signal.removeEventListener("abort", onAbort);
              resolve();
            }, ms);
            signal.addEventListener("abort", onAbort, { once: true });
          });
        },
      });
      let projectionTimer: ReturnType<typeof setInterval> | null = null;
      const stopProjectionTimer = () => {
        if (projectionTimer !== null) clearInterval(projectionTimer);
        projectionTimer = null;
      };
      return {
        async start() {
          const rev = await productionSpecProjection.requestForSession(
            sessionId,
            "resume",
          );
          if (rev !== null) {
            // The manager owns the existing per-session lease before it calls
            // start. Publish the resume intent before the event stream can
            // deliver a prompt boundary.
            await productionSpecProjection.runOnce(sessionId);
            projectionTimer = setInterval(() => {
              void productionSpecProjection.runOnce(sessionId).catch((error) => {
                log.error({ sessionId, error }, "spec projection scanner failed");
              });
            }, 250);
          }
          try {
            await listener.run();
          } finally {
            stopProjectionTimer();
            await productionSpecProjection.waitForIdle(sessionId);
          }
        },
        async stop() {
          stopProjectionTimer();
          await listener.stop();
          // stop() drains every queued consumer before it returns. Wait after
          // that drain so no new projection run can start before lease release.
          await productionSpecProjection.waitForIdle(sessionId);
        },
      };
    },
  });
}
