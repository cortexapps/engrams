import { afterEach, describe, expect, test } from "bun:test";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";

import { createClient, createRouterTransport } from "@connectrpc/connect";

import { SessionService } from "../../gen/engram/app/v1/session_pb.ts";
import { traceRequest } from "../../server.ts";
import { tracingInterceptor } from "../connect.ts";
import type { OtlpSpan } from "../otlp.ts";
import {
  makeOtlpHttpSink,
  parseTraceparent,
  setTracer,
  Tracer,
  tracer,
  type SpanSink,
} from "../trace.ts";

const CALLER = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

function capture(): { sink: SpanSink; spans: OtlpSpan[] } {
  const spans: OtlpSpan[] = [];
  return { spans, sink: { push: (s) => spans.push(s), flush: async () => {} } };
}

/** Make one RPC through an in-process Connect transport carrying the tracing
 * interceptor, returning the `traceparent` the server received. */
async function outboundTraceparent(): Promise<string | null> {
  let received: string | null = null;
  const transport = createRouterTransport(
    (router) => {
      router.service(SessionService, {
        getSession(_req, ctx) {
          received = ctx.requestHeader.get("traceparent");
          return {};
        },
      });
    },
    { transport: { interceptors: [tracingInterceptor] } },
  );
  await createClient(SessionService, transport).getSession({ sessionId: "s1" });
  return received;
}

afterEach(() => setTracer(new Tracer(null)));

describe("trace context", () => {
  test("survives an await chain and reaches the outbound traceparent", async () => {
    const { sink, spans } = capture();
    setTracer(new Tracer(sink));
    const header = await tracer.inSpan("outer", { parent: parseTraceparent(CALLER) }, async () => {
      await Bun.sleep(1);
      await Promise.resolve();
      await new Promise((resolve) => setTimeout(resolve, 1));
      return outboundTraceparent();
    });

    const sent = parseTraceparent(header);
    expect(sent?.traceId).toBe("4bf92f3577b34da6a3ce929d0e0e4736");
    const client = spans.find((s) => s.name === "engram.app.v1.SessionService/GetSession");
    const outer = spans.find((s) => s.name === "outer");
    expect(outer?.parentSpanId).toBe("00f067aa0ba902b7");
    expect(client?.parentSpanId).toBe(outer?.spanId);
    expect(sent?.spanId).toBe(client?.spanId);
  });

  test("follows a node:http request through its body events", async () => {
    const { sink, spans } = capture();
    setTracer(new Tracer(sink));
    let seen: string | undefined;
    const server = createServer((req: IncomingMessage, res: ServerResponse) =>
      traceRequest(req, res, (r, w) => {
        r.on("data", () => {});
        r.on("end", () => {
          setTimeout(() => {
            seen = tracer.traceparent();
            w.end("ok");
          }, 1);
        });
      }),
    );
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    try {
      const { port } = server.address() as AddressInfo;
      const response = await fetch(`http://127.0.0.1:${port}/rpc/engram.app.v1.SessionService/SendPrompt`, {
        method: "POST",
        headers: { traceparent: CALLER },
        body: "{}",
      });
      expect(await response.text()).toBe("ok");
      await Bun.sleep(10);
    } finally {
      server.close();
    }

    expect(parseTraceparent(seen)?.traceId).toBe("4bf92f3577b34da6a3ce929d0e0e4736");
    const serverSpan = spans.find((s) => s.name === "engram.app.v1.SessionService/SendPrompt");
    expect(serverSpan?.parentSpanId).toBe("00f067aa0ba902b7");
    expect(parseTraceparent(seen)?.spanId).toBe(serverSpan?.spanId);
  });

  test("a disabled tracer sends no header and records nothing", async () => {
    const header = await tracer.inSpan("outer", {}, () => outboundTraceparent());
    expect(header).toBeNull();
    expect(tracer.enabled).toBe(false);
  });

  test("a failing span records its error and rethrows", async () => {
    const { sink, spans } = capture();
    setTracer(new Tracer(sink));
    await expect(
      tracer.inSpan("fails", {}, async () => {
        throw new Error("boom");
      }),
    ).rejects.toThrow("boom");
    expect(spans[0]?.status?.code).toBe(2);
  });
});

describe("parseTraceparent", () => {
  test("accepts a W3C header and refuses malformed or zero ids", () => {
    expect(parseTraceparent(CALLER)).toEqual({
      traceId: "4bf92f3577b34da6a3ce929d0e0e4736",
      spanId: "00f067aa0ba902b7",
    });
    expect(parseTraceparent("garbage")).toBeUndefined();
    expect(parseTraceparent(undefined)).toBeUndefined();
    expect(parseTraceparent(`00-${"0".repeat(32)}-00f067aa0ba902b7-01`)).toBeUndefined();
  });
});

describe("makeOtlpHttpSink", () => {
  test("posts batched spans to <endpoint>/v1/traces as OTLP JSON", async () => {
    const posts: { url: string; body: unknown }[] = [];
    const post = async (url: string, init: RequestInit) => {
      posts.push({ url, body: JSON.parse(String(init.body)) });
      return new Response(null, { status: 200 });
    };
    const sink = makeOtlpHttpSink("http://collector:4318/", "svc", post);
    setTracer(new Tracer(sink));
    await tracer.inSpan("one", {}, () => {});
    await sink.flush();

    expect(posts).toHaveLength(1);
    expect(posts[0]?.url).toBe("http://collector:4318/v1/traces");
    const body = posts[0]?.body as {
      resourceSpans: { resource: { attributes: unknown[] }; scopeSpans: { spans: OtlpSpan[] }[] }[];
    };
    expect(body.resourceSpans[0]?.resource.attributes).toContainEqual({
      key: "service.name",
      value: { stringValue: "svc" },
    });
    expect(body.resourceSpans[0]?.scopeSpans[0]?.spans[0]?.name).toBe("one");
  });
});
