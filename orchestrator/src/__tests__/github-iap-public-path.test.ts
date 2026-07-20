import { describe, expect, test } from "bun:test";

import { isIapPublicPath } from "../auth/iap-bridge.ts";

describe("GitHub webhook IAP exemption", () => {
  test("is exact-path public, including with a query string", () => {
    expect(isIapPublicPath("/api/v1/integrations/github/events")).toBe(true);
    expect(isIapPublicPath("/api/v1/integrations/github/events?delivery=1"))
      .toBe(true);
    expect(isIapPublicPath("/api/v1/integrations/github/events/extra"))
      .toBe(false);
  });
});
