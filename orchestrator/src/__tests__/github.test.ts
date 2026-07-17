import { createHmac } from "node:crypto";
import { describe, expect, test } from "bun:test";

import { verifyGithubSignature } from "../integrations/github.ts";

describe("verifyGithubSignature", () => {
  const secret = "correct horse battery staple";
  const body = JSON.stringify({ zen: "Keep it logically awesome." });
  const signature = `sha256=${createHmac("sha256", secret).update(body).digest("hex")}`;

  test("accepts the documented sha256 HMAC", () => {
    expect(verifyGithubSignature(secret, body, signature)).toBe(true);
  });

  test("rejects wrong secrets, tampering, missing headers, and wrong lengths", () => {
    expect(verifyGithubSignature("wrong", body, signature)).toBe(false);
    expect(verifyGithubSignature(secret, `${body}x`, signature)).toBe(false);
    expect(verifyGithubSignature(secret, body, undefined)).toBe(false);
    expect(verifyGithubSignature(secret, body, "sha256=short")).toBe(false);
  });

  test("malformed signatures never throw", () => {
    expect(() => verifyGithubSignature(secret, body, "💥")).not.toThrow();
  });
});
