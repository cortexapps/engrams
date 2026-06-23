/**
 * Connector-logo serve route (redesign): GET /api/v1/integrations/:provider/logo.
 *   - 401 when unauthenticated
 *   - 404 when no logo (web falls back to the monogram)
 *   - serves the bytes + media type when present, with a nosniff header
 */

import { expect, test, describe } from "bun:test";

import { makeConnectorLogoRoute } from "../routes/connector-logo.ts";
import type { ConnectorLogoStore } from "../db/connector-logos.ts";

function fakeStore(seed: Record<string, { mediaType: string; data: Buffer }> = {}): ConnectorLogoStore {
  const rows = new Map(Object.entries(seed));
  return {
    async get(provider) {
      const r = rows.get(provider);
      return r ? { provider, mediaType: r.mediaType, data: r.data, updatedAt: new Date(0) } : null;
    },
    async put() {},
    async delete(provider) {
      return rows.delete(provider);
    },
    async listProviders() {
      return [...rows.keys()];
    },
  };
}

const authed = async () => ({ user: { id: "u" } });
const anon = async () => null;

describe("connector-logo serve route", () => {
  test("401 when unauthenticated", async () => {
    const app = makeConnectorLogoRoute({ store: fakeStore(), getSession: anon });
    const res = await app.request("/api/v1/integrations/github/logo");
    expect(res.status).toBe(401);
  });

  test("404 when the provider has no logo", async () => {
    const app = makeConnectorLogoRoute({ store: fakeStore(), getSession: authed });
    const res = await app.request("/api/v1/integrations/github/logo");
    expect(res.status).toBe(404);
  });

  test("serves the bytes + content-type when present", async () => {
    const data = Buffer.from([1, 2, 3, 4]);
    const app = makeConnectorLogoRoute({
      store: fakeStore({ github: { mediaType: "image/png", data } }),
      getSession: authed,
    });
    const res = await app.request("/api/v1/integrations/github/logo");
    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toBe("image/png");
    expect(res.headers.get("x-content-type-options")).toBe("nosniff");
    expect([...new Uint8Array(await res.arrayBuffer())]).toEqual([1, 2, 3, 4]);
  });
});
