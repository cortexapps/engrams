import { expect, test } from "bun:test";
import { Hono } from "hono";
import { makeHttpLog } from "../http-log.ts";
test("HTTP logs retain method path status and time without callback query or request body", async () => {
  const records: unknown[] = [];
  let now = 100;
  const app = new Hono();
  app.use(
    makeHttpLog(
      (record) => records.push(record),
      () => now,
    ),
  );
  app.get("/oauth/mcp", (c) => {
    now = 125;
    return c.text("login required", 401);
  });
  app.post("/credential", (c) => c.text("ok"));
  await app.request(
    "https://app.example.test/oauth/mcp?code=secret-code&state=secret-state&iss=https://issuer.example",
  );
  await app.request("https://app.example.test/credential", {
    method: "POST",
    headers: { authorization: "Bearer private" },
    body: "entered-secret",
  });
  expect(records[0]).toEqual({ method: "GET", path: "/oauth/mcp", status: 401, durationMs: 25 });
  expect(records[1]).toEqual({ method: "POST", path: "/credential", status: 200, durationMs: 0 });
  const text = JSON.stringify(records);
  for (const secret of [
    "secret-code",
    "secret-state",
    "issuer.example",
    "entered-secret",
    "private",
  ])
    expect(text).not.toContain(secret);
});
