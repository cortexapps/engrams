/**
 * /api/v1/auth-config tests (bun test).
 *
 * The endpoint exposes the public auth posture to the SPA login page. It reads
 * `config` live per-request, so we toggle `config.iapAudiences` (a plain object,
 * not frozen — same pattern as iap-bridge.test.ts) to exercise both postures.
 * No DB or network required.
 */

import { expect, test, describe, afterEach } from "bun:test";
import authConfigRoute from "../routes/auth-config.ts";
import { config } from "../config.ts";

function setIapAudiences(value: string[]): void {
  (config as { iapAudiences: string[] }).iapAudiences = value;
}

describe("GET /api/v1/auth-config", () => {
  const saved = config.iapAudiences;
  afterEach(() => setIapAudiences(saved));

  test("IAP off → password auth + signup enabled", async () => {
    setIapAudiences([]);
    const res = await authConfigRoute.request("/api/v1/auth-config");
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({ passwordAuth: true, signup: true });
  });

  test("IAP on → password auth + signup disabled", async () => {
    setIapAudiences(["/projects/123/apps/test"]);
    const res = await authConfigRoute.request("/api/v1/auth-config");
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({ passwordAuth: false, signup: false });
  });
});
