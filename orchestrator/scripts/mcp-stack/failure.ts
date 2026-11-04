export class RunnerFailure extends Error {
  constructor(
    readonly code:
      | "INVALID_CONTEXT"
      | "ASSERTION_FAILED"
      | "NEEDS_CONTEXT"
      | "TRANSPORT_FAILED",
    readonly seam: string,
  ) {
    super(`${code}:${seam}`);
  }
}
