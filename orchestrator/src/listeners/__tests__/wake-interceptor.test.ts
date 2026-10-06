import { describe, expect, test } from "bun:test";
import { createContextValues, type UnaryRequest, type UnaryResponse } from "@connectrpc/connect";
import { create } from "@bufbuild/protobuf";
import pino from "pino";

import {
  GetSessionRequestSchema,
  GetSessionResponseSchema,
  SendPromptRequestSchema,
  SendPromptResponseSchema,
  SessionService,
} from "../../gen/engram/app/v1/session_pb.ts";
import { makeListenerWakeInterceptor, wakeTarget } from "../wake-interceptor.ts";

const log = pino({ level: "silent" });

function sendPrompt(sessionId: string) {
  const req: UnaryRequest<typeof SendPromptRequestSchema, typeof SendPromptResponseSchema> = {
    stream: false,
    service: SessionService,
    method: SessionService.method.sendPrompt,
    requestMethod: "POST",
    url: "http://control-plane.invalid/engram.app.v1.SessionService/SendPrompt",
    signal: new AbortController().signal,
    header: new Headers(),
    contextValues: createContextValues(),
    message: create(SendPromptRequestSchema, { sessionId }),
  };
  const res: UnaryResponse<typeof SendPromptRequestSchema, typeof SendPromptResponseSchema> = {
    stream: false,
    service: SessionService,
    method: SessionService.method.sendPrompt,
    header: new Headers(),
    trailer: new Headers(),
    message: create(SendPromptResponseSchema),
  };
  return { req, res };
}

describe("wakeTarget", () => {
  test("a resuming unary session rpc names its session", () => {
    expect(wakeTarget(sendPrompt("s1").req)).toBe("s1");
  });

  test("reads, streams, other services, and an empty id wake nothing", () => {
    const get: UnaryRequest<typeof GetSessionRequestSchema, typeof GetSessionResponseSchema> = {
      ...sendPrompt("s1").req,
      method: SessionService.method.getSession,
      message: create(GetSessionRequestSchema, { sessionId: "s1" }),
    };
    expect(wakeTarget(get)).toBeUndefined();
    expect(wakeTarget({ ...sendPrompt("s1").req, stream: true })).toBeUndefined();
    expect(
      wakeTarget({ ...sendPrompt("s1").req, service: { typeName: "engram.app.v1.FleetService" } }),
    ).toBeUndefined();
    expect(wakeTarget(sendPrompt("").req)).toBeUndefined();
  });
});

describe("makeListenerWakeInterceptor", () => {
  test("wakes the session after the rpc returns, in that order", async () => {
    const order: string[] = [];
    const interceptor = makeListenerWakeInterceptor(async (sessionId) => {
      order.push(`wake:${sessionId}`);
    }, log);
    const { req, res } = sendPrompt("s1");
    const result = await interceptor(async () => {
      order.push("rpc");
      return res;
    })(req);
    expect(result).toBe(res);
    expect(order).toEqual(["rpc", "wake:s1"]);
  });

  test("a failed rpc still wakes, and the failure propagates", async () => {
    const woken: string[] = [];
    const interceptor = makeListenerWakeInterceptor(async (sessionId) => {
      woken.push(sessionId);
    }, log);
    await expect(
      interceptor(async () => {
        throw new Error("upstream down");
      })(sendPrompt("s1").req),
    ).rejects.toThrow("upstream down");
    expect(woken).toEqual(["s1"]);
  });

  test("a failed wake never fails the rpc", async () => {
    const interceptor = makeListenerWakeInterceptor(async () => {
      throw new Error("pg down");
    }, log);
    const { req, res } = sendPrompt("s1");
    expect(await interceptor(async () => res)(req)).toBe(res);
  });

  test("the rpc returns without waiting on the wake", async () => {
    let settleWake: (() => void) | undefined;
    const interceptor = makeListenerWakeInterceptor(
      () => new Promise<void>((resolve) => (settleWake = resolve)),
      log,
    );
    const { req, res } = sendPrompt("s1");
    // Resolves while the wake is still pending: a stalled database never
    // holds the caller's response.
    expect(await interceptor(async () => res)(req)).toBe(res);
    expect(settleWake).toBeDefined();
    settleWake!();
  });

  test("a read does not wake", async () => {
    let woken = 0;
    const interceptor = makeListenerWakeInterceptor(async () => {
      woken++;
    }, log);
    const { req, res } = sendPrompt("s1");
    const get: UnaryRequest<typeof GetSessionRequestSchema, typeof GetSessionResponseSchema> = {
      ...req,
      method: SessionService.method.getSession,
      message: create(GetSessionRequestSchema, { sessionId: "s1" }),
    };
    await interceptor(async () => res)(get);
    expect(woken).toBe(0);
  });
});
