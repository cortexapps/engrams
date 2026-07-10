/**
 * Generic passthrough forwarder with authz gate (ADR 0051 Task 18).
 *
 * For every method in every PassthroughSpec:
 *   1. Verify the caller has a valid better-auth session (Unauthenticated if not).
 *   2. Look up the per-method PolicyEntry (PermissionDenied if absent — fail-closed).
 *   3. Build a CASL ability for the session's user.
 *   4. For session-scoped methods: resolve the session owner and check
 *      ownership.  Returns NotFound for unowned/unknown sessions — anti-
 *      enumeration behaviour (ADR §6).
 *   5. Forward the RPC to the control-plane upstream and pipe response
 *      headers/trailers back to the caller.
 *
 * GetSession signature of better-auth is injectable for tests
 * (`getSession?: GetSession`). When omitted, getSessionFromHeaders resolves
 * the real better-auth session (cookie or API key, ADR 0086).
 *
 * Excluded from this layer (served elsewhere):
 *   - StreamEvents / GetArtifact → Hono routes (Task 20)
 *   - ShellRelayService.Relay   → WS route (Task 21)
 *   - TaskService.*             → native impl (Task 19)
 */

import { ConnectError, Code } from "@connectrpc/connect";
import type { ConnectRouter, Transport, HandlerContext } from "@connectrpc/connect";
import type { DescService } from "@bufbuild/protobuf";
import { subject } from "@casl/ability";
import { POLICY, policyKey } from "../authz/policy-map.ts";
import { abilityFor } from "../authz/ability.ts";
import { resolveSessionOwner } from "../authz/resolve.ts";
import { getSessionFromHeaders } from "../auth/session.ts";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface PassthroughSpec {
  service: DescService;
  /**
   * If provided, only forward methods whose proto-name (PascalCase) is in
   * this list. Omit to forward all methods in the service.
   */
  methods?: string[];
}

/**
 * Injected getSession implementation. The default is getSessionFromHeaders
 * (auth/session.ts); tests inject a stub.
 */
export type GetSession = (
  headers: Headers,
) => Promise<{
  user: { id: string; role?: string | null; email?: string | null };
} | null>;

/**
 * Injected session-owner resolver. The default is the DB-backed
 * resolveSessionOwner; tests inject a stub that doesn't need a DB.
 */
export type ResolveOwner = (sessionId: string) => Promise<string | null>;

/** Optional per-method pre-flight, keyed by policyKey ("ImageService.DisableImage").
 *  Runs AFTER the authz gate and BEFORE the upstream forward. Throws to block. */
export type Preflight = (req: unknown, ctx: HandlerContext) => Promise<void>;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/**
 * Headers that the gRPC upstream sends but that MUST NOT be forwarded to the
 * browser-facing Connect response. The ConnectRouter sets content-type for the
 * outbound protocol (application/json) and gRPC framing headers are not
 * meaningful (and actively harmful) on a Connect response.
 */
const BLOCKED_RESPONSE_HEADERS = new Set([
  "content-type",        // ConnectRouter owns this — forwarding application/grpc breaks browser parsing
  "grpc-status",         // gRPC trailer, not a Connect response header
  "grpc-message",        // gRPC trailer
  "grpc-status-details-bin", // gRPC trailer
  "transfer-encoding",   // hop-by-hop, must not be forwarded
]);

/**
 * Copy headers from a source Headers object into a destination, excluding
 * protocol-level headers that must not leak from the upstream gRPC response
 * into the downstream Connect response.
 */
function copyHeaders(src: Headers | undefined, dst: Headers): void {
  if (!src) return;
  src.forEach((value, key) => {
    if (!BLOCKED_RESPONSE_HEADERS.has(key.toLowerCase())) {
      dst.set(key, value);
    }
  });
}

/**
 * Build a Web-standard Headers object from a HandlerContext's requestHeader.
 * getSessionFromHeaders takes a Headers instance; the context provides one
 * already.
 */
function headersOf(ctx: HandlerContext): Headers {
  return ctx.requestHeader;
}

/**
 * Build clean upstream headers from an inbound request.
 *
 * The inbound Connect/gRPC-Web request carries protocol-specific headers
 * (content-type: application/json, connect-protocol-version, etc.) that must
 * NOT be forwarded to the outbound gRPC/HTTP-2 call — Tonic rejects them as
 * NGHTTP2_PROTOCOL_ERROR. Only safe, application-level headers (x-request-id,
 * x-trace-id, etc.) are forwarded; the gRPC transport adds its own content-type
 * and the bearer interceptor injects the Authorization header.
 */
function upstreamHeaders(inbound: Headers): Headers {
  const out = new Headers();
  // Allowlist of safe headers to propagate to the control plane.
  // Content-type, connect-*, accept, and other protocol headers must be
  // omitted — they belong to the inbound Connect protocol, not gRPC.
  const ALLOWED_PREFIXES = ["x-request-id", "x-trace-id", "x-b3-", "traceparent", "tracestate"];
  inbound.forEach((value, key) => {
    const lower = key.toLowerCase();
    if (ALLOWED_PREFIXES.some((p) => lower.startsWith(p))) {
      out.set(key, value);
    }
  });
  return out;
}

// ---------------------------------------------------------------------------
// Main registration
// ---------------------------------------------------------------------------

/**
 * Register passthrough handlers on the ConnectRouter.
 *
 * @param router        The ConnectRouter to register handlers on.
 * @param specs         Which services (and optionally which methods) to forward.
 * @param upstream      The control-plane Transport to forward calls to.
 * @param getSession    Optional override for better-auth session resolution.
 * @param resolveOwner  Optional override for session-owner DB lookup (tests).
 * @param preflight     Optional per-method pre-flight hooks, keyed by policyKey.
 *                      Each runs after the authz gate and before the forward.
 */
export function registerPassthrough(
  router: ConnectRouter,
  specs: PassthroughSpec[],
  upstream: Transport,
  getSession?: GetSession,
  resolveOwner?: ResolveOwner,
  preflight?: Record<string, Preflight>,
): void {
  const resolveSession: GetSession =
    getSession ??
    getSessionFromHeaders;

  const ownerResolver: ResolveOwner = resolveOwner ?? resolveSessionOwner;

  for (const { service, methods } of specs) {
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const impl: Record<string, any> = {};

    for (const m of service.methods) {
      // Skip methods not in the allowlist.
      if (methods && !methods.includes(m.name)) continue;

      // Only support unary and server-streaming (bidi/client-streaming live
      // on dedicated routes).
      if (
        m.methodKind !== "unary" &&
        m.methodKind !== "server_streaming"
      ) {
        continue;
      }

      // Per-method policy key (e.g. "ImageService.DisableImage"). Hoisted out
      // of the gate closure so the unary handler can use it to look up an
      // optional pre-flight hook.
      const key = policyKey(service.typeName, m);

      /**
       * Authz gate — runs before any upstream call.
       * Throws a ConnectError on failure.
       */
      const gate = async (req: unknown, ctx: HandlerContext): Promise<void> => {
        // 1. Authenticate.
        const session = await resolveSession(headersOf(ctx));
        if (!session) {
          throw new ConnectError("unauthenticated", Code.Unauthenticated);
        }

        // 2. Look up policy (fail-closed: no entry → denied).
        const entry = POLICY[key];
        if (!entry) {
          throw new ConnectError(
            `no policy for ${key}`,
            Code.PermissionDenied,
          );
        }

        // 3. Build ability.
        const ability = abilityFor({
          id: session.user.id,
          role: session.user.role ?? "user",
        });

        // 4. Config guard: a Session-subject entry without sessionIdField would
        // fall into the flat string-subject branch below where CASL ignores
        // conditions and returns true for any member — latent fail-open.
        // Catch it loud so a policy-map edit doesn't silently open the gate.
        if (entry.subject === "Session" && !entry.sessionIdField) {
          throw new ConnectError(
            `policy misconfiguration: Session-subject entry for ${key} has no sessionIdField`,
            Code.Internal,
          );
        }

        // 5. Ownership check (session-scoped methods).
        if (entry.sessionIdField) {
          const sid = (req as Record<string, string>)[entry.sessionIdField];
          if (!sid) {
            // No session_id in the request → treat as not found.
            throw new ConnectError("not found", Code.NotFound);
          }

          const ownerId = await ownerResolver(sid);

          // Anti-enumeration: return NotFound even when the session exists
          // but is not owned by the caller — don't confirm existence.
          if (
            !ability.can(
              entry.action,
              subject("Session", { createdByUserId: ownerId }),
            )
          ) {
            throw new ConnectError("not found", Code.NotFound);
          }
        } else {
          // Non-session-scoped (Fleet, admin Image, etc.).
          if (!ability.can(entry.action, entry.subject)) {
            throw new ConnectError("forbidden", Code.PermissionDenied);
          }
        }
      };

      // Register handler.
      if (m.methodKind === "unary") {
        impl[m.localName] = async (req: unknown, ctx: HandlerContext) => {
          await gate(req, ctx);
          const pf = preflight?.[key];
          if (pf) await pf(req, ctx);
          const res = await upstream.unary(
            // eslint-disable-next-line @typescript-eslint/no-explicit-any
            m as any,
            ctx.signal,
            undefined,
            // Use clean headers — inbound Connect headers (content-type: application/json,
            // connect-protocol-version, etc.) must NOT be forwarded to the outbound gRPC
            // transport or Tonic returns NGHTTP2_PROTOCOL_ERROR. The transport's
            // bearerInterceptor injects Authorization; only safe tracing headers are passed.
            upstreamHeaders(ctx.requestHeader),
            // eslint-disable-next-line @typescript-eslint/no-explicit-any
            req as any,
            ctx.values,
          );
          copyHeaders(res.header, ctx.responseHeader);
          copyHeaders(res.trailer, ctx.responseTrailer);
          return res.message;
        };
      } else {
        // server_streaming
        impl[m.localName] = async function* (req: unknown, ctx: HandlerContext) {
          await gate(req, ctx);
          const res = await upstream.stream(
            // eslint-disable-next-line @typescript-eslint/no-explicit-any
            m as any,
            ctx.signal,
            undefined,
            // Same header-scrubbing rationale as the unary case above.
            upstreamHeaders(ctx.requestHeader),
            // eslint-disable-next-line @typescript-eslint/no-explicit-any
            (async function* () { yield req as any; })(),
            ctx.values,
          );
          copyHeaders(res.header, ctx.responseHeader);
          for await (const msg of res.message) {
            yield msg;
          }
        };
      }
    }

    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    router.service(service as any, impl);
  }
}
