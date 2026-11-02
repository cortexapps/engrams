import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { test } from "node:test";
import {
  Client,
  StreamableHTTPClientTransport,
} from "@modelcontextprotocol/client";
import { startMcpFixture } from "./fixtures/server.ts";

for (const version of ["2026-07-28", "2025-11-25", "2025-06-18"] as const) {
  for (const reply of ["json", "sse"] as const) {
    test(`SDK discovers pages and calls tools over TLS: ${version} ${reply}`, async (t) => {
      const fixture = await startMcpFixture({ version, reply });
      t.after(() => fixture.close());
      const client = new Client(
        { name: "fixture-client", version: "1" },
        {
          versionNegotiation: { mode: "auto" },
        },
      );
      t.after(() => client.close());
      const transport = new StreamableHTTPClientTransport(
        new URL(fixture.url),
        { fetch: fixture.fetch },
      );
      await client.connect(transport);
      const first = await client.request({ method: "tools/list", params: {} });
      assert.equal(first.tools[0]?.name, "read_counter");
      assert.equal(
        first.tools[0]?.inputSchema.$schema,
        "http://json-schema.org/draft-07/schema#",
      );
      assert.equal(first.nextCursor, "page-2");
      const second = await client.listTools({ cursor: first.nextCursor });
      assert.equal(second.tools[0]?.name, "write_counter");
      assert.equal(
        second.tools[0]?.inputSchema.$schema,
        "https://json-schema.org/draft/2020-12/schema",
      );
      assert.equal(second.nextCursor, undefined);
      const read = await client.callTool({
        name: "read_counter",
        arguments: {},
      });
      assert.deepEqual(read.structuredContent, { count: 0 });
      const written = await client.callTool({
        name: "write_counter",
        arguments: { delta: 2 },
      });
      assert.deepEqual(written.structuredContent, { count: 2 });
      assert.deepEqual(written.content, [{ type: "text", text: "count=2" }]);
      assert.equal(written.isError, false);
      assert.equal(fixture.writes, 1);
      assert.equal(fixture.reads, 1);
      if (version === "2026-07-28") {
        assert.equal(
          fixture.requests.some((r) => r.method === "initialize"),
          false,
        );
        assert.equal(transport.sessionId, undefined);
      } else {
        assert.ok(fixture.requests.some((r) => r.method === "initialize"));
        assert.equal(transport.sessionId, "fixture-session-1");
        assert.equal(
          fixture.requests.find((r) => r.method === "tools/call")?.sessionId,
          transport.sessionId,
        );
      }
    });
  }
}

test("default trust rejects the private fixture CA; explicit test trust accepts it", async (t) => {
  const fixture = await startMcpFixture();
  t.after(() => fixture.close());
  await assert.rejects(
    fetch(`${fixture.origin}/.well-known/oauth-protected-resource`),
  );
  const response = await fixture.fetch(
    `${fixture.origin}/.well-known/oauth-protected-resource`,
  );
  assert.equal(response.status, 200);
});

test("catalog revision changes description and schema on the wire", async (t) => {
  const fixture = await startMcpFixture();
  t.after(() => fixture.close());
  const before = await rpc(fixture, "tools/list");
  fixture.setCatalogRevision(2);
  const after = await rpc(fixture, "tools/list");
  assert.notDeepEqual(before.body, after.body);
  assert.match(JSON.stringify(after.body), /revision 2/);
  assert.match(JSON.stringify(after.body), /required_tag/);
});

test("legacy transport sessions reject a different credential identity", async (t) => {
  const fixture = await startMcpFixture({ version: "2025-11-25" });
  t.after(() => fixture.close());
  const initialized = await rpc(
    fixture,
    "initialize",
    {
      protocolVersion: "2025-11-25",
      capabilities: {},
      clientInfo: { name: "test", version: "1" },
    },
    { authorization: "Bearer account-one" },
  );
  const sessionId = initialized.response.headers.get("mcp-session-id");
  assert.equal(sessionId, "fixture-session-1");
  const response = await rpc(
    fixture,
    "tools/list",
    {},
    {
      authorization: "Bearer account-two",
      "mcp-session-id": sessionId!,
    },
  );
  assert.equal(response.response.status, 404);
});

test("modern metadata header mismatches fail before a write", async (t) => {
  const fixture = await startMcpFixture();
  t.after(() => fixture.close());
  const response = await rpc(
    fixture,
    "tools/call",
    { name: "write_counter", arguments: { delta: 1 } },
    {
      "mcp-method": "tools/list",
    },
  );
  assert.equal(response.response.status, 400);
  assert.equal(fixture.writes, 0);
});

test("held calls release explicitly and transport abort records cancellation", async (t) => {
  const fixture = await startMcpFixture({ holdCalls: true, reply: "sse" });
  t.after(() => fixture.close());
  const controller = new AbortController();
  const pending = fixture.fetch(fixture.url, {
    method: "POST",
    headers: modernHeaders("tools/call", "read_counter"),
    body: JSON.stringify(
      message("tools/call", { name: "read_counter", arguments: {} }),
    ),
    signal: controller.signal,
  });
  const response = await pending;
  const body = response.text();
  await fixture.waitForCall();
  controller.abort();
  await assert.rejects(body);
  await fixture.waitForCancellation();
  assert.equal(fixture.cancellations, 1);
  assert.equal(fixture.writes, 0);
  const released = rpc(fixture, "tools/call", {
    name: "read_counter",
    arguments: {},
  });
  await fixture.waitForCall();
  fixture.releaseCalls();
  assert.equal((await released).response.status, 200);
});

for (const mode of [
  "malformed",
  "oversize",
  "image",
  "audio",
  "resource",
  "resource_link",
  "error",
] as const) {
  test(`fixture serves ${mode} output without hiding an upstream write`, async (t) => {
    const fixture = await startMcpFixture({ response: mode });
    t.after(() => fixture.close());
    const response = await fixture.fetch(fixture.url, {
      method: "POST",
      headers: modernHeaders("tools/call", "write_counter"),
      body: JSON.stringify(
        message("tools/call", {
          name: "write_counter",
          arguments: { delta: 1 },
        }),
      ),
    });
    const body = await response.text();
    assert.equal(response.status, 200);
    assert.equal(fixture.writes, 1);
    if (mode === "malformed") assert.throws(() => JSON.parse(body));
    else if (mode === "oversize")
      assert.ok(Buffer.byteLength(body) > 2 * 1024 * 1024);
    else if (mode === "error")
      assert.equal(JSON.parse(body).result.isError, true);
    else assert.equal(JSON.parse(body).result.content[0].type, mode);
  });
}

test("OAuth metadata, PKCE code exchange, rotation and replay use real TLS", async (t) => {
  const fixture = await startMcpFixture({ requireAuth: true });
  t.after(() => fixture.close());
  const unauthorized = await rpc(fixture, "tools/list");
  assert.equal(unauthorized.response.status, 401);
  assert.match(
    unauthorized.response.headers.get("www-authenticate")!,
    /resource_metadata=/,
  );
  const resource = await (
    await fixture.fetch(
      `${fixture.origin}/.well-known/oauth-protected-resource`,
    )
  ).json();
  assert.equal(resource.resource, fixture.url);
  assert.deepEqual(resource.authorization_servers, [fixture.origin]);
  const metadata = await (
    await fixture.fetch(
      `${fixture.origin}/.well-known/oauth-authorization-server`,
    )
  ).json();
  assert.equal(metadata.issuer, fixture.origin);
  assert.equal(metadata.authorization_response_iss_parameter_supported, true);
  assert.deepEqual(metadata.code_challenge_methods_supported, ["S256"]);
  const verifier = "v".repeat(43);
  const authorizationUrl = new URL(metadata.authorization_endpoint);
  authorizationUrl.search = new URLSearchParams({
    response_type: "code",
    client_id: "fixture-public",
    redirect_uri: `${fixture.origin}/callback`,
    state: "state-one",
    code_challenge: createHash("sha256").update(verifier).digest("base64url"),
    code_challenge_method: "S256",
    resource: fixture.url,
    scope: "tools:read tools:write",
  }).toString();
  const authorization = await fixture.fetch(authorizationUrl, {
    redirect: "manual",
  });
  assert.equal(authorization.status, 302);
  const callback = new URL(authorization.headers.get("location")!);
  assert.equal(callback.searchParams.get("iss"), metadata.issuer);
  assert.equal(callback.searchParams.get("state"), "state-one");
  const code = callback.searchParams.get("code")!;
  const grant = {
    grant_type: "authorization_code",
    code,
    code_verifier: verifier,
    client_id: "fixture-public",
    redirect_uri: `${fixture.origin}/callback`,
    resource: fixture.url,
  };
  assert.equal(
    (await token(fixture, { ...grant, code_verifier: "wrong" })).status,
    400,
  );
  assert.equal(
    (await token(fixture, { ...grant, resource: "https://other.example/mcp" }))
      .status,
    400,
  );
  const exchanged = await token(fixture, grant);
  assert.equal(exchanged.status, 200);
  const tokens = await exchanged.json();
  assert.equal(tokens.access_token, "fixture-access-1");
  assert.equal((await token(fixture, grant)).status, 400);
  assert.equal((await fixture.fetch(authorizationUrl)).status, 400);
  const authorized = await rpc(
    fixture,
    "tools/list",
    {},
    { authorization: `Bearer ${tokens.access_token}` },
  );
  assert.equal(authorized.response.status, 200);
  const refresh = {
    grant_type: "refresh_token",
    refresh_token: tokens.refresh_token,
    client_id: "fixture-public",
    resource: fixture.url,
  };
  const rotated = await (await token(fixture, refresh)).json();
  assert.equal(rotated.refresh_token, "fixture-refresh-2");
  assert.equal((await token(fixture, refresh)).status, 400);
  assert.equal(
    (
      await rpc(
        fixture,
        "tools/list",
        {},
        { authorization: `Bearer ${tokens.access_token}` },
      )
    ).response.status,
    401,
  );
});

test("OAuth fixture can return a mismatched callback issuer", async (t) => {
  const fixture = await startMcpFixture({
    callbackIssuer: "https://other.example",
  });
  t.after(() => fixture.close());
  const url = new URL(`${fixture.origin}/authorize`);
  url.search = new URLSearchParams({
    response_type: "code",
    client_id: "fixture-public",
    redirect_uri: `${fixture.origin}/callback`,
    state: "issuer-case",
    resource: fixture.url,
    code_challenge: "c".repeat(43),
    code_challenge_method: "S256",
  }).toString();
  const response = await fixture.fetch(url);
  assert.equal(
    new URL(response.headers.get("location")!).searchParams.get("iss"),
    "https://other.example",
  );
});

test("OAuth rejects missing PKCE, bad redirect, unknown client, and scope expansion", async (t) => {
  const fixture = await startMcpFixture();
  t.after(() => fixture.close());
  const url = new URL(`${fixture.origin}/authorize`);
  const params = {
    response_type: "code",
    client_id: "fixture-public",
    redirect_uri: `${fixture.origin}/callback`,
    state: "state",
    resource: fixture.url,
    code_challenge: "c".repeat(43),
    code_challenge_method: "S256",
    scope: "tools:read",
  };
  for (const change of [
    { code_challenge_method: "plain" },
    { redirect_uri: "https://other.example/callback" },
    { client_id: "unknown" },
    { scope: "admin" },
    { resource: "https://other.example" },
  ]) {
    url.search = new URLSearchParams({ ...params, ...change }).toString();
    assert.equal((await fixture.fetch(url)).status, 400);
  }
});

function message(method: string, params: Record<string, unknown> = {}) {
  return {
    jsonrpc: "2.0",
    id: 1,
    method,
    params: {
      ...params,
      _meta: {
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {
          name: "fixture-client",
          version: "1",
        },
        "io.modelcontextprotocol/clientCapabilities": {},
      },
    },
  };
}
function modernHeaders(method: string, name?: string) {
  return {
    "content-type": "application/json",
    accept: "application/json, text/event-stream",
    "mcp-protocol-version": "2026-07-28",
    "mcp-method": method,
    ...(name ? { "mcp-name": name } : {}),
  };
}
async function rpc(
  fixture: Awaited<ReturnType<typeof startMcpFixture>>,
  method: string,
  params: Record<string, unknown> = {},
  headers: Record<string, string> = {},
) {
  const response = await fixture.fetch(fixture.url, {
    method: "POST",
    headers: {
      ...modernHeaders(
        method,
        typeof params.name === "string" ? params.name : undefined,
      ),
      ...headers,
    },
    body: JSON.stringify(message(method, params)),
  });
  const text = await response.text();
  const data = text.startsWith("data: ") ? text.slice(6).trim() : text;
  return { response, body: data ? JSON.parse(data) : undefined };
}
function token(
  fixture: Awaited<ReturnType<typeof startMcpFixture>>,
  params: Record<string, string>,
) {
  return fixture.fetch(`${fixture.origin}/token`, {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams(params).toString(),
  });
}

test("binary HTTP output retains the committed write counter", async (t) => {
  const fixture = await startMcpFixture({ response: "binary" });
  t.after(() => fixture.close());
  const response = await fixture.fetch(fixture.url, {
    method: "POST",
    headers: modernHeaders("tools/call", "write_counter"),
    body: JSON.stringify(
      message("tools/call", { name: "write_counter", arguments: { delta: 1 } }),
    ),
  });
  assert.equal(
    response.headers.get("content-type"),
    "application/octet-stream",
  );
  assert.deepEqual(
    new Uint8Array(await response.arrayBuffer()),
    new Uint8Array([0, 255, 128, 1]),
  );
  assert.equal(fixture.writes, 1);
});

test("legacy cancellation stops only the matching held call", async (t) => {
  const fixture = await startMcpFixture({
    version: "2025-11-25",
    holdCalls: true,
    reply: "sse",
  });
  t.after(() => fixture.close());
  const initialized = await rpc(fixture, "initialize", {
    protocolVersion: "2025-11-25",
    capabilities: {},
    clientInfo: { name: "fixture-client", version: "1" },
  });
  const headers = {
    ...modernHeaders("tools/call", "read_counter"),
    "mcp-session-id": initialized.response.headers.get("mcp-session-id")!,
  };
  const first = await fixture.fetch(fixture.url, {
    method: "POST",
    headers,
    body: JSON.stringify({
      ...message("tools/call", { name: "read_counter", arguments: {} }),
      id: 10,
    }),
  });
  const firstBody = first.text().catch(() => "canceled");
  const second = await fixture.fetch(fixture.url, {
    method: "POST",
    headers,
    body: JSON.stringify({
      ...message("tools/call", { name: "read_counter", arguments: {} }),
      id: 20,
    }),
  });
  const secondBody = second.text().catch(() => "canceled");
  const canceled = await fixture.fetch(fixture.url, {
    method: "POST",
    headers,
    body: JSON.stringify({
      jsonrpc: "2.0",
      method: "notifications/cancelled",
      params: { requestId: 10 },
    }),
  });
  assert.equal(canceled.status, 202);
  await fixture.waitForCancellation();
  fixture.releaseCalls();
  assert.equal(await firstBody, "canceled");
  assert.match(await secondBody, /"id":20/);
  assert.equal(fixture.cancellations, 1);
});

test("legacy cancellation cannot cross a transport session with the same request ID", async (t) => {
  const fixture = await startMcpFixture({
    version: "2025-11-25",
    holdCalls: true,
    reply: "sse",
  });
  t.after(() => fixture.close());
  const firstSession = await rpc(fixture, "initialize", {
    protocolVersion: "2025-11-25",
    capabilities: {},
    clientInfo: { name: "fixture-client", version: "1" },
  });
  const secondSession = await rpc(fixture, "initialize", {
    protocolVersion: "2025-11-25",
    capabilities: {},
    clientInfo: { name: "fixture-client", version: "1" },
  });
  const firstHeaders = {
    ...modernHeaders("tools/call", "read_counter"),
    "mcp-session-id": firstSession.response.headers.get("mcp-session-id")!,
  };
  const secondHeaders = {
    ...modernHeaders("tools/call", "read_counter"),
    "mcp-session-id": secondSession.response.headers.get("mcp-session-id")!,
  };
  const body = JSON.stringify({
    ...message("tools/call", { name: "read_counter", arguments: {} }),
    id: 10,
  });
  const first = await fixture.fetch(fixture.url, {
    method: "POST",
    headers: firstHeaders,
    body,
  });
  const firstBody = first.text().catch(() => "canceled");
  const second = await fixture.fetch(fixture.url, {
    method: "POST",
    headers: secondHeaders,
    body,
  });
  const secondBody = second.text().catch(() => "canceled");
  await fixture.fetch(fixture.url, {
    method: "POST",
    headers: firstHeaders,
    body: JSON.stringify({
      jsonrpc: "2.0",
      method: "notifications/cancelled",
      params: { requestId: 10 },
    }),
  });
  await fixture.waitForCancellation();
  fixture.releaseCalls();
  assert.equal(await firstBody, "canceled");
  assert.match(await secondBody, /"id":10/);
  assert.equal(fixture.cancellations, 1);
});
