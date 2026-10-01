/**
 * GET /api/auth/device — the device flow's verify leg needs a session
 * (the `hooks.before` guard in auth/better-auth.ts).
 *
 * The plugin answers this endpoint for anyone. In `iap` mode the bridge's
 * fail-closed 401 used to be the only thing in front of it, so a deployment
 * with no IAP would hand an anonymous caller the status of any user code it
 * guessed. Runs against the REAL auth instance; an anonymous request is
 * refused before the database is touched, so no Postgres is needed.
 */

import { expect, test, describe } from "bun:test";
import { auth } from "../auth/better-auth.ts";

const BASE = "http://127.0.0.1:8787/api/auth";

describe("GET /api/auth/device", () => {
  test("anonymous → 401, before any lookup", async () => {
    const res = await auth.handler(new Request(`${BASE}/device?user_code=ABCD1234`));
    expect(res.status).toBe(401);
  });

  test("a junk session cookie is still anonymous → 401", async () => {
    const res = await auth.handler(
      new Request(`${BASE}/device?user_code=ABCD1234`, {
        headers: { cookie: "better-auth.session_token=forged.signature" },
      }),
    );
    expect(res.status).toBe(401);
  });
});
