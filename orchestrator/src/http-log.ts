import type { MiddlewareHandler } from "hono";
export interface HttpLogRecord {
  method: string;
  path: string;
  status: number;
  durationMs: number;
}
/** Query strings can contain authorization codes. Never log URLs or bodies. */
export function makeHttpLog(
  write: (record: HttpLogRecord) => void,
  now: () => number = Date.now,
): MiddlewareHandler {
  return async (context, next) => {
    const start = now();
    await next();
    write({
      method: context.req.method,
      path: context.req.path,
      status: context.res.status,
      durationMs: Math.max(0, now() - start),
    });
  };
}
