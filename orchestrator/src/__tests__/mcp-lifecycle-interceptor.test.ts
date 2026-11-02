import { expect, test } from "bun:test";
import { createContextValues, type UnaryRequest, type UnaryResponse } from "@connectrpc/connect";
import { create } from "@bufbuild/protobuf";
import {
  InterruptRequestSchema,
  InterruptResponseSchema,
  SessionService,
} from "../gen/engram/app/v1/session_pb.ts";
import { makeMcpLifecycleInterceptor } from "../mcp/lifecycle-interceptor.ts";

test("trusted interrupt waits for durable invalidation before forwarding", async () => {
  const order: string[] = [];
  const req: UnaryRequest<typeof InterruptRequestSchema, typeof InterruptResponseSchema> = {
    stream: false,
    service: SessionService,
    method: SessionService.method.interrupt,
    requestMethod: "POST",
    url: "http://example.test",
    signal: new AbortController().signal,
    header: new Headers(),
    contextValues: createContextValues(),
    message: create(InterruptRequestSchema, { sessionId: "s1" }),
  };
  const res: UnaryResponse<typeof InterruptRequestSchema, typeof InterruptResponseSchema> = {
    stream: false,
    service: SessionService,
    method: SessionService.method.interrupt,
    header: new Headers(),
    trailer: new Headers(),
    message: create(InterruptResponseSchema),
  };
  const interceptor = makeMcpLifecycleInterceptor(async (id, cause) => {
    order.push(`${id}:${cause}`);
  });
  await interceptor(async () => {
    order.push("forward");
    return res;
  })(req);
  expect(order).toEqual(["s1:interrupt", "forward"]);
});

test("invalidation failure stops the lifecycle request", async () => {
  const req: UnaryRequest<typeof InterruptRequestSchema, typeof InterruptResponseSchema> = {
    stream: false,
    service: SessionService,
    method: SessionService.method.interrupt,
    requestMethod: "POST",
    url: "http://example.test",
    signal: new AbortController().signal,
    header: new Headers(),
    contextValues: createContextValues(),
    message: create(InterruptRequestSchema, { sessionId: "s1" }),
  };
  let forwarded = false;
  const interceptor = makeMcpLifecycleInterceptor(async () => {
    throw Error("store unavailable");
  });
  await expect(
    interceptor(async () => {
      forwarded = true;
      throw Error("unexpected forward");
    })(req),
  ).rejects.toThrow("store unavailable");
  expect(forwarded).toBe(false);
});
