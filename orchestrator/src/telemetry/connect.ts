/**
 * Connect interceptors that carry the orchestrator's trace across RPCs.
 */

import type { Interceptor } from "@connectrpc/connect";

import { SPAN_KIND_CLIENT, tracer } from "./trace.ts";

/** A client span per outbound RPC, sent as a W3C `traceparent` header so the
 * server's spans join this request's trace. For a streaming RPC the span ends
 * when the response headers arrive. */
export const tracingInterceptor: Interceptor = (next) => (req) =>
  tracer.inSpan(
    `${req.service.typeName}/${req.method.name}`,
    {
      kind: SPAN_KIND_CLIENT,
      attributes: {
        "rpc.system": "grpc",
        "rpc.service": req.service.typeName,
        "rpc.method": req.method.name,
      },
    },
    () => {
      const traceparent = tracer.traceparent();
      if (traceparent) req.header.set("traceparent", traceparent);
      return next(req);
    },
  );
