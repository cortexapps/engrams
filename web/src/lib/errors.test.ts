import { describe, it, expect } from "vitest";
import { ConnectError, Code } from "@connectrpc/connect";
import { errorMessage } from "./errors";

describe("errorMessage", () => {
  it("returns the clean upstream message for a ConnectError (no [code] prefix)", () => {
    // Exactly the enable-image incident: the coordinator's registry-auth
    // message must reach the user, not collapse to a status code.
    const msg =
      "registry pull for `reg.example/x:tag` failed: OCI distribution error: " +
      "Not authorized. Check that a matching registry credential exists.";
    const err = new ConnectError(msg, Code.InvalidArgument);
    const out = errorMessage(err);
    expect(out).toBe(msg);
    // The `[invalid_argument]` prefix that String(err)/toString() adds must
    // NOT leak into the UI text.
    expect(out).not.toContain("[invalid_argument]");
    expect(out).toContain("Not authorized");
  });

  it("does not collapse a non-Internal ConnectError to a bare HTTP status", () => {
    const err = new ConnectError("boom from upstream", Code.FailedPrecondition);
    expect(errorMessage(err)).toBe("boom from upstream");
  });

  it("falls back to .message for a plain Error", () => {
    expect(errorMessage(new Error("plain failure"))).toBe("plain failure");
  });

  it("stringifies non-Error throwables", () => {
    expect(errorMessage("just a string")).toContain("just a string");
  });
});
