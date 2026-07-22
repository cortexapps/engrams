import { describe, expect, test } from "bun:test";

import { BodyTooLargeError, readBoundedBody } from "../bounded-body.ts";

function streamingRequest(
  chunks: readonly Uint8Array[],
  headers: Record<string, string> = {},
): Request {
  return new Request("http://localhost/hook", {
    method: "POST",
    headers,
    body: new ReadableStream<Uint8Array>({
      start(controller) {
        for (const chunk of chunks) controller.enqueue(chunk);
        controller.close();
      },
    }),
  });
}

const bytes = (value: string) => new TextEncoder().encode(value);

describe("readBoundedBody", () => {
  test("reads a normal body with no Content-Length header", async () => {
    const body = await readBoundedBody(
      streamingRequest([bytes("hello "), bytes("world")]),
      32,
    );
    expect(new TextDecoder().decode(body)).toBe("hello world");
  });

  test("ignores a malformed Content-Length header", async () => {
    const body = await readBoundedBody(
      streamingRequest([bytes("ok")], { "content-length": "not-a-number" }),
      8,
    );
    expect(new TextDecoder().decode(body)).toBe("ok");
  });

  test("rejects an understated oversized chunked body", async () => {
    const request = streamingRequest(
      [new Uint8Array(5), new Uint8Array(6)],
      { "content-length": "1" },
    );
    await expect(readBoundedBody(request, 10)).rejects.toBeInstanceOf(BodyTooLargeError);
  });

  test("rejects a valid oversized declared length without trusting it as the only limit", async () => {
    await expect(readBoundedBody(
      streamingRequest([bytes("small")], { "content-length": "11" }),
      10,
    )).rejects.toBeInstanceOf(BodyTooLargeError);
  });

  test("rejects an oversized stream without a declared length", async () => {
    await expect(
      readBoundedBody(streamingRequest([new Uint8Array(4), new Uint8Array(4)]), 7),
    ).rejects.toBeInstanceOf(BodyTooLargeError);
  });

  test("accepts a body exactly at the limit", async () => {
    const body = await readBoundedBody(streamingRequest([new Uint8Array(8)]), 8);
    expect(body.byteLength).toBe(8);
  });
});
