import { request } from "node:https";
import { TEST_CA } from "./tls.ts";

// Test-only HTTPS adapter: explicit private CA, streaming bodies, and abort support.
// It never changes process TLS settings and never follows redirects.
export const fixtureFetch: typeof fetch = async (input, init) => {
  const source = input instanceof Request ? input : new Request(input, init);
  const headers = new Headers(init?.headers ?? source.headers);
  const signal = init?.signal ?? source.signal;
  const body = init?.body ?? (source.body ? await source.text() : undefined);
  if (body !== undefined && body !== null && typeof body !== "string") {
    throw new Error("Fixture fetch accepts string request bodies only");
  }
  return new Promise<Response>((resolve, reject) => {
    const req = request(source.url, {
      method: init?.method ?? source.method,
      headers: Object.fromEntries(headers),
      ca: TEST_CA,
      signal,
      agent: false,
    });
    req.on("error", reject);
    req.on("response", (res) => {
      const responseHeaders = new Headers();
      for (const [key, value] of Object.entries(res.headers)) {
        if (value !== undefined)
          responseHeaders.set(
            key,
            Array.isArray(value) ? value.join(", ") : value,
          );
      }
      const stream = new ReadableStream<Uint8Array>({
        start(controller) {
          res.on("data", (chunk: Buffer) => controller.enqueue(chunk));
          res.on("end", () => controller.close());
          res.on("error", (error) => controller.error(error));
        },
        cancel() {
          res.destroy();
        },
      });
      const status = res.statusCode ?? 500;
      resolve(
        new Response([204, 205, 304].includes(status) ? null : stream, {
          status,
          headers: responseHeaders,
        }),
      );
    });
    req.end(body);
  });
};
