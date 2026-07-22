/** Streaming request-body reader for unauthenticated ingress. */

export const MAX_WEBHOOK_BODY_BYTES = 2 * 1024 * 1024;

export class BodyTooLargeError extends Error {
  constructor(readonly limitBytes: number) {
    super(`request body exceeds ${limitBytes} bytes`);
    this.name = "BodyTooLargeError";
  }
}

/**
 * Read at most `limitBytes` from the actual request stream. Content-Length is
 * deliberately irrelevant: it is optional, untrusted, and absent on chunked
 * requests. The reader is cancelled as soon as the first byte over the limit
 * is observed and oversized bytes are never copied into the accumulator.
 */
export async function readBoundedBody(
  request: Request,
  limitBytes = MAX_WEBHOOK_BODY_BYTES,
): Promise<Uint8Array> {
  if (!Number.isSafeInteger(limitBytes) || limitBytes < 0) {
    throw new RangeError("limitBytes must be a non-negative safe integer");
  }
  const declaredLength = request.headers.get("content-length");
  if (declaredLength !== null && /^\d+$/.test(declaredLength)) {
    const parsed = Number(declaredLength);
    if (Number.isSafeInteger(parsed) && parsed > limitBytes) {
      throw new BodyTooLargeError(limitBytes);
    }
  }
  if (request.body === null) return new Uint8Array();

  const reader = request.body.getReader();
  const chunks: Uint8Array[] = [];
  let consumed = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      consumed += value.byteLength;
      if (consumed > limitBytes) {
        try {
          await reader.cancel("request body too large");
        } catch {
          // The rejection remains 413 even if the transport cannot be cancelled.
        }
        throw new BodyTooLargeError(limitBytes);
      }
      chunks.push(value);
    }
  } finally {
    reader.releaseLock();
  }

  const body = new Uint8Array(consumed);
  let offset = 0;
  for (const chunk of chunks) {
    body.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return body;
}
