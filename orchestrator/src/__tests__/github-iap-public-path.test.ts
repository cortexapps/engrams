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

  test("dynamic hooks require POST plus exactly one valid registration slug", () => {
    expect(isIapPublicPath("/api/v1/hooks/my-hook?delivery=1", "POST")).toBe(true);
    expect(isIapPublicPath("/api/v1/hooks/my-hook", "GET")).toBe(false);
    expect(isIapPublicPath("/api/v1/hooks/my-hook/extra", "POST")).toBe(false);
    expect(isIapPublicPath("/api/v1/hooks/Bad_Slug", "POST")).toBe(false);
  });
});
