import { APICallError, RetryError } from "ai";

/** An API rejection a retry cannot fix: an `APICallError` the SDK marks
 * non-retryable (a bad key, an unknown model). The SDK's own retries surface
 * as a `RetryError`; its last cause decides. A rate limit, a 5xx, or a
 * network fault is not permanent. */
export function isPermanentModelError(err: unknown): boolean {
  const cause = RetryError.isInstance(err) ? err.lastError : err;
  return APICallError.isInstance(cause) && !cause.isRetryable;
}
