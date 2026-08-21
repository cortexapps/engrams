/** Typed failure for integration actions (ADR 0119 D5). `permanent` drives
 * the engine's per-block retry policy: transient (5xx / 429 / 408 /
 * transport) may retry; everything else must not.
 */

export class IntegrationActionError extends Error {
  constructor(
    message: string,
    readonly permanent: boolean,
    readonly status?: number,
  ) {
    super(message);
    this.name = "IntegrationActionError";
  }
}

export function statusIsTransient(status: number): boolean {
  return status >= 500 || status === 429 || status === 408;
}
