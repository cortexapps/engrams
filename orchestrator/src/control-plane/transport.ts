/**
 * Control-plane gRPC transport (ADR 0051 §5).
 *
 * Machine identity: one trusted caller (the orchestrator), one static credential.
 * The bearer is set once at startup and attached to every outbound RPC via an
 * interceptor — no per-request minting, no context keys.
 *
 * Keepalive options (ADR §9.4): detect half-open connections so leases don't
 * ghost. Options are top-level on GrpcTransportOptions (which extends
 * NodeHttp2TransportOptions via Http2SessionOptions).
 *
 *   pingIntervalMs    — send PING every 20 s when a stream is open
 *   pingTimeoutMs     — treat connection as dead if PING unanswered after 10 s
 *   pingIdleConnection — also PING connections with no open streams (so an
 *                        idle orchestrator detects half-open H2 sessions early)
 */

import { createGrpcTransport } from "@connectrpc/connect-node";
import type { Interceptor, Transport } from "@connectrpc/connect";
import { config } from "../config.ts";

// Machine identity (ADR §5): one trusted caller, one static credential.
export const bearerInterceptor: Interceptor = (next) => (req) => {
  req.header.set("authorization", `Bearer ${config.controlPlaneBearer}`);
  return next(req);
};

/**
 * Factory that lets tests inject a custom baseUrl without touching the
 * config singleton. The real `controlPlaneTransport` below uses the
 * singleton's URL.
 */
export function makeTransport(baseUrl: string, bearer?: string): Transport {
  // No override → reuse the exported bearerInterceptor (one definition of
  // "attach the machine credential"); an override builds a scoped variant.
  const interceptor: Interceptor =
    bearer === undefined
      ? bearerInterceptor
      : (next) => (req) => {
          req.header.set("authorization", `Bearer ${bearer}`);
          return next(req);
        };

  return createGrpcTransport({
    baseUrl,
    interceptors: [interceptor],
    // ADR §9.4: detect half-open connections so leases don't ghost.
    // These are top-level options from Http2SessionOptions (not under nodeOptions).
    pingIntervalMs: 20_000,
    pingTimeoutMs: 10_000,
    pingIdleConnection: true,
  });
}

/**
 * Singleton transport for all outbound control-plane RPCs.
 * Created once at module load; reused for the lifetime of the process.
 */
export const controlPlaneTransport: Transport = makeTransport(
  config.controlPlaneGrpcUrl,
  config.controlPlaneBearer,
);
