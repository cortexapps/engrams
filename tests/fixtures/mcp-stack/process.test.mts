import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import {
  chmodSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { request } from "node:https";
import { createServer } from "node:net";
import { resolve } from "node:path";
import { test } from "node:test";
import { getCACertificates, setDefaultCACertificates } from "node:tls";
import {
  Client,
  StreamableHTTPClientTransport,
} from "../../../mcp-worker/node_modules/@modelcontextprotocol/client/dist/index.mjs";
import { setTimeout as delay } from "node:timers/promises";
import {
  TEST_CA,
  TEST_CERT,
  TEST_KEY,
} from "../../../mcp-worker/test/fixtures/tls.ts";
import { loadConfig } from "./config.mts";

const entry = resolve("tests/fixtures/mcp-stack/main.mts");
const base = resolve("var/mcp-stack-fixture");
const observerToken = "neutral_observer_token_0000000000000000";
const apiKey = "neutral_resource_key_0000000000000000";
async function freePort() {
  const server = createServer();
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  assert.ok(address && typeof address !== "string");
  const port = address.port;
  await new Promise<void>((resolve, reject) =>
    server.close((e) => (e ? reject(e) : resolve())),
  );
  return port;
}
async function setup() {
  const root = mkdtempSync(`${base}/process-`);
  chmodSync(root, 0o700);
  const port = await freePort();
  let controlPort = await freePort();
  while (controlPort === port) controlPort = await freePort();
  const origin = `https://localhost:${port}`;
  const control = `https://localhost:${controlPort}`;
  for (const [name, value] of Object.entries({
    "cert.pem": TEST_CERT,
    "key.pem": TEST_KEY,
    "ca.pem": TEST_CA,
    "observer.token": observerToken,
    "api.key": apiKey,
  }))
    writeFileSync(`${root}/${name}`, value, { mode: 0o600 });
  const config = {
    version: 1,
    runRoot: root,
    listen: { host: "127.0.0.1", port },
    advertised: {
      resource: origin,
      issuer: origin,
      authorization: origin,
      token: origin,
      redirectUri: `${origin}/integrations/mcp/callback`,
      authorities: [`localhost:${port}`],
    },
    tls: {
      certificateFile: `${root}/cert.pem`,
      keyFile: `${root}/key.pem`,
      caFile: `${root}/ca.pem`,
    },
    control: {
      port: controlPort,
      origin: control,
      tokenFile: `${root}/observer.token`,
    },
    protocol: "2026-07-28",
    case: {
      reply: "json",
      response: "normal",
      holdCalls: false,
      requireAuth: true,
    },
    apiKeyFile: `${root}/api.key`,
  };
  const path = `${root}/config.json`;
  const save = () =>
    writeFileSync(path, JSON.stringify(config), { mode: 0o600 });
  save();
  return { root, origin, control, config, path, save };
}
function wire(
  url: string | URL,
  options: {
    body?: string;
    method?: string;
    headers?: Record<string, string>;
    signal?: AbortSignal;
  } = {},
) {
  return new Promise<{
    status: number;
    headers: import("node:http").IncomingHttpHeaders;
    text: string;
    json: () => any;
  }>((resolve, reject) => {
    const req = request(
      url,
      {
        ca: TEST_CA,
        servername: new URL(url).hostname,
        family: 4,
        agent: false,
        method: options.method ?? "GET",
        headers: options.headers ?? {},
        ...(options.signal ? { signal: options.signal } : {}),
      },
      (res) => {
        const chunks: Buffer[] = [];
        let size = 0;
        res.on("data", (chunk) => {
          size += chunk.length;
          if (size > 3 * 1024 * 1024) res.destroy();
          else chunks.push(chunk);
        });
        res.on("error", reject);
        res.on("end", () => {
          const text = Buffer.concat(chunks).toString();
          resolve({
            status: res.statusCode!,
            headers: res.headers,
            text,
            json: () => JSON.parse(text),
          });
        });
      },
    );
    req.on("error", reject);
    req.end(options.body);
  });
}
async function start(
  f: Awaited<ReturnType<typeof setup>>,
  expectedReady = true,
) {
  const child = spawn(process.execPath, [entry, f.path], {
    cwd: process.cwd(),
    env: { PATH: "/usr/bin:/bin", TMPDIR: f.root },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk) => {
    stdout += chunk;
    if (stdout.length > 4096) child.kill("SIGKILL");
  });
  child.stderr.on("data", (chunk) => {
    stderr += chunk;
    if (stderr.length > 4096) child.kill("SIGKILL");
  });
  const exited = once(child, "close");
  const watchdog = setTimeout(() => child.kill("SIGKILL"), 29000);
  const stop = async (signal: NodeJS.Signals = "SIGTERM") => {
    if (child.exitCode === null && child.signalCode === null)
      child.kill(signal);
    const escalation = setTimeout(() => child.kill("SIGKILL"), 2000);
    try {
      await exited;
    } finally {
      clearTimeout(escalation);
      clearTimeout(watchdog);
    }
    assert.equal(
      child.signalCode,
      signal === "SIGKILL" ? "SIGKILL" : null,
      "graceful exit must not need kill",
    );
    if (signal !== "SIGKILL")
      assert.equal(child.exitCode, expectedReady ? 0 : 1);
    for (const secret of [
      observerToken,
      apiKey,
      "fixture-access-",
      "fixture-refresh-",
      "fixture-code-",
    ])
      assert.equal((stdout + stderr).includes(secret), false);
  };
  try {
    if (expectedReady) {
      for (let n = 0; !stdout.includes('"kind":"ready"'); ++n) {
        if (n === 150 || child.exitCode !== null)
          throw new Error(`Fixture did not become ready: ${stderr}`);
        await delay(10);
      }
    } else {
      await exited;
      assert.equal(stdout, "");
      assert.equal(stderr.trim(), '{"version":1,"kind":"startup_failed"}');
    }
  } catch (error) {
    await stop();
    throw error;
  }
  return { child, stop, exited };
}
function control(
  f: Awaited<ReturnType<typeof setup>>,
  route: string,
  body?: object,
  auth = observerToken,
) {
  return wire(`${f.control}/v1/${route}`, {
    headers: {
      authorization: `Bearer ${auth}`,
      "content-type": "application/json",
    },
    ...(body ? { method: "POST", body: JSON.stringify(body) } : {}),
  });
}
async function poll(
  f: Awaited<ReturnType<typeof setup>>,
  test: (value: any) => boolean,
) {
  for (let n = 0; n < 100; ++n) {
    const value = (await control(f, "observe")).json();
    if (test(value)) return value;
    await delay(10);
  }
  throw new Error("Observer barrier timeout");
}
function rpc(
  f: Awaited<ReturnType<typeof setup>>,
  name: string,
  id: number | string,
  auth = apiKey,
  signal?: AbortSignal,
) {
  return wire(`${f.origin}/mcp`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      authorization: `Bearer ${auth}`,
      "mcp-protocol-version": "2026-07-28",
      "mcp-method": "tools/call",
      "mcp-name": name,
    },
    body: JSON.stringify({
      jsonrpc: "2.0",
      id,
      method: "tools/call",
      params: {
        name,
        arguments: name === "write_counter" ? { delta: 2 } : {},
        _meta: { "io.modelcontextprotocol/protocolVersion": "2026-07-28" },
      },
    }),
    ...(signal ? { signal } : {}),
  });
}

test("standalone TLS process commits held writes before reply and preserves duplicate executions on reopen", async () => {
  const f = await setup();
  let running: Awaited<ReturnType<typeof start>> | undefined;
  try {
    running = await start(f);
    assert.equal((await control(f, "ready")).json().ready, true);
    const trust = getCACertificates();
    setDefaultCACertificates([...trust, TEST_CA]);
    const client = new Client(
      { name: "standalone-fixture-client", version: "1" },
      { versionNegotiation: { mode: "auto" } },
    );
    try {
      await client.connect(
        new StreamableHTTPClientTransport(new URL(`${f.origin}/mcp`), {
          requestInit: { headers: { authorization: `Bearer ${apiKey}` } },
        }),
      );
      const first = await client.request({ method: "tools/list", params: {} });
      assert.equal(first.tools[0]?.name, "read_counter");
      assert.equal(
        (await client.listTools({ cursor: first.nextCursor! })).tools[0]?.name,
        "write_counter",
      );
      assert.deepEqual(
        (await client.callTool({ name: "read_counter", arguments: {} }))
          .structuredContent,
        { count: 0 },
      );
    } finally {
      await client.close();
      setDefaultCACertificates(trust);
    }
    assert.equal((await control(f, "observe", undefined, "wrong")).status, 401);
    assert.equal((await control(f, "unknown", {})).status, 404);
    assert.equal((await control(f, "case", { arbitrary: true })).status, 400);
    assert.equal((await rpc(f, "write_counter", "same", "wrong")).status, 401);
    assert.equal((await control(f, "case", { holdCalls: true })).status, 200);
    let completed = false;
    const held = rpc(f, "write_counter", "same").then((value) => {
      completed = true;
      return value;
    });
    const observed = await poll(
      f,
      (s) => s.effects.executions === 1 && s.held === 1,
    );
    assert.equal(completed, false);
    assert.equal(observed.effects.count, 2);
    assert.equal(
      JSON.parse(readFileSync(`${f.root}/effects.json`, "utf8")).state
        .executions,
      1,
    );
    assert.equal((await control(f, "case", { response: "error" })).status, 409);
    await control(f, "release", {});
    assert.equal((await held).json().result.structuredContent.count, 2);
    await running.stop("SIGKILL");
    running = undefined;
    running = await start(f);
    assert.equal((await control(f, "observe")).json().effects.executions, 1);
    assert.equal(
      (await rpc(f, "write_counter", "same")).json().result.structuredContent
        .count,
      4,
    );
    assert.equal((await control(f, "observe")).json().effects.executions, 2);
    await control(f, "case", { holdCalls: true });
    const canceled = rpc(f, "write_counter", "cancel").catch(() => "canceled");
    await poll(f, (s) => s.held === 1 && s.effects.executions === 3);
    await control(f, "cancel", {});
    assert.equal(await canceled, "canceled");
    await poll(f, (s) => s.cancellations === 1);
    const stopped = rpc(f, "write_counter", "shutdown").catch(() => "closed");
    await poll(f, (s) => s.held === 1 && s.effects.executions === 4);
    await running.stop();
    running = undefined;
    assert.equal(await stopped, "closed");
    assert.equal(
      JSON.parse(readFileSync(`${f.root}/effects.json`, "utf8")).state
        .executions,
      4,
    );
  } finally {
    if (running) await running.stop();
    rmSync(f.root, { recursive: true, force: true });
  }
});

test("standalone OAuth enforces PKCE/resource, rotates refresh and exposes only neutral observations", async () => {
  const f = await setup();
  let running: Awaited<ReturnType<typeof start>> | undefined;
  try {
    running = await start(f);
    const metadata = (
      await wire(`${f.origin}/.well-known/oauth-authorization-server`)
    ).json();
    const resource = (
      await wire(`${f.origin}/.well-known/oauth-protected-resource`)
    ).json();
    assert.equal(metadata.issuer, f.origin);
    assert.equal(resource.resource, `${f.origin}/mcp`);
    const verifier = "v".repeat(43);
    const authorize = new URL(metadata.authorization_endpoint);
    authorize.search = new URLSearchParams({
      response_type: "code",
      client_id: "fixture-public",
      redirect_uri: f.config.advertised.redirectUri,
      state: "neutral-state",
      code_challenge: createHash("sha256").update(verifier).digest("base64url"),
      code_challenge_method: "S256",
      resource: `${f.origin}/mcp`,
      scope: "tools:read tools:write",
    }).toString();
    authorize.searchParams.set(
      "redirect_uri",
      `${f.config.advertised.redirectUri}/other`,
    );
    assert.equal((await wire(authorize)).status, 400);
    authorize.searchParams.set("redirect_uri", f.config.advertised.redirectUri);
    const authorization = await wire(authorize);
    assert.equal(authorization.status, 302);
    const callback = new URL(String(authorization.headers.location));
    assert.equal(callback.searchParams.get("iss"), metadata.issuer);
    assert.equal(callback.pathname, "/integrations/mcp/callback");
    const token = (body: Record<string, string>) =>
      wire(metadata.token_endpoint, {
        method: "POST",
        headers: { "content-type": "application/x-www-form-urlencoded" },
        body: new URLSearchParams(body).toString(),
      });
    const grant = {
      grant_type: "authorization_code",
      code: callback.searchParams.get("code")!,
      code_verifier: verifier,
      client_id: "fixture-public",
      redirect_uri: f.config.advertised.redirectUri,
      resource: `${f.origin}/mcp`,
    };
    assert.equal((await token({ ...grant, code_verifier: "bad" })).status, 400);
    assert.equal(
      (await token({ ...grant, resource: "https://other.example/mcp" })).status,
      400,
    );
    assert.equal(
      (await token({ ...grant, redirect_uri: `${grant.redirect_uri}/other` }))
        .status,
      400,
    );
    const first = (await token(grant)).json();
    assert.equal(first.fixture_account, "account-one");
    assert.equal(
      (await rpc(f, "read_counter", 1, first.access_token)).status,
      200,
    );
    await control(f, "oauth", { account: "account-two", scope: "tools:read" });
    const refresh = {
      grant_type: "refresh_token",
      refresh_token: first.refresh_token,
      client_id: "fixture-public",
      resource: `${f.origin}/mcp`,
    };
    const second = (await token(refresh)).json();
    assert.notEqual(second.refresh_token, first.refresh_token);
    assert.equal(second.fixture_account, "account-two");
    assert.equal(second.scope, "tools:read");
    assert.equal((await token(refresh)).status, 400);
    assert.equal(
      (await rpc(f, "read_counter", 2, first.access_token)).status,
      401,
    );
    assert.equal(
      (await rpc(f, "read_counter", 3, second.access_token)).status,
      200,
    );
    const observed = await control(f, "observe");
    assert.deepEqual(observed.json().oauth, {
      account: "account-two",
      scope: "tools:read",
      authorizations: 1,
      exchanges: 1,
      rotations: 1,
      rejections: 5,
    });
    for (const secret of [
      first.access_token,
      first.refresh_token,
      second.access_token,
      second.refresh_token,
      grant.code,
      apiKey,
      observerToken,
    ])
      assert.equal(observed.text.includes(secret), false);
    assert.equal((await control(f, "oauth", { scope: "admin" })).status, 400);
    await control(f, "oauth", { issuer: "mismatch" });
    authorize.searchParams.set("state", "issuer-mismatch");
    const mismatched = await wire(authorize);
    assert.equal(
      new URL(String(mismatched.headers.location)).searchParams.get("iss"),
      "https://other.example",
    );
  } finally {
    if (running) await running.stop();
    rmSync(f.root, { recursive: true, force: true });
  }
});

test("config, authority, and durable corruption fail closed without secret output", async () => {
  const f = await setup();
  let running: Awaited<ReturnType<typeof start>> | undefined;
  try {
    for (const mutate of [
      (c: typeof f.config & { unknown?: boolean }) => {
        c.unknown = true;
      },
      (c: typeof f.config) => {
        c.version = 2;
      },
      (c: typeof f.config) => {
        c.advertised.issuer = "http://localhost:1234";
      },
      (c: typeof f.config) => {
        c.advertised.token = `https://other.example:${c.listen.port}`;
        c.advertised.authorities.push(`other.example:${c.listen.port}`);
      },
      (c: typeof f.config) => {
        c.advertised.authorities = [];
      },
      (c: typeof f.config) => {
        c.advertised.redirectUri += "?";
      },
      (c: typeof f.config) => {
        c.advertised.redirectUri += "#";
      },
      (c: typeof f.config) => {
        c.advertised.redirectUri = "https://user:pass@localhost/callback";
      },
    ]) {
      const copy = structuredClone(f.config);
      mutate(copy);
      writeFileSync(f.path, JSON.stringify(copy));
      assert.throws(() => loadConfig(f.path));
    }
    // One real process rejection proves the entrypoint also fails closed.
    running = await start(f, false);
    await running.stop();
    running = undefined;
    f.save();
    running = await start(f);
    const wrong = await wire(`${f.origin}/mcp`, {
      headers: { host: "other.example" },
    });
    assert.equal(wrong.status, 421);
    await running.stop();
    running = undefined;
    writeFileSync(`${f.root}/effects.json`, "truncated");
    running = await start(f, false);
    await running.stop();
    running = undefined;
  } finally {
    if (running) await running.stop();
    rmSync(f.root, { recursive: true, force: true });
  }
});

test("config accepts separate DNS origins with matching supplied SANs and exact authorities", async () => {
  const f = await setup();
  try {
    const names = [
      "localhost",
      "mcp.fixture.example",
      "issuer.fixture.example",
      "authorization.fixture.example",
      "token.fixture.example",
      "callback.fixture.example",
    ];
    execFileSync(
      "/usr/bin/openssl",
      [
        "req",
        "-x509",
        "-new",
        "-key",
        `${f.root}/key.pem`,
        "-out",
        `${f.root}/network.pem`,
        "-days",
        "2",
        "-subj",
        "/CN=mcp.fixture.example",
        "-addext",
        `subjectAltName=${names.map((n) => `DNS:${n}`).join(",")}`,
      ],
      {
        env: { PATH: "/usr/bin:/bin", TMPDIR: f.root },
        stdio: "ignore",
        timeout: 2000,
      },
    );
    f.config.tls.certificateFile = `${f.root}/network.pem`;
    f.config.tls.caFile = `${f.root}/network.pem`;
    const advertised = f.config.advertised;
    advertised.resource = `https://mcp.fixture.example:${f.config.listen.port}`;
    advertised.issuer = `https://issuer.fixture.example:${f.config.listen.port}`;
    advertised.authorization = `https://authorization.fixture.example:${f.config.listen.port}`;
    advertised.token = `https://token.fixture.example:${f.config.listen.port}`;
    advertised.redirectUri =
      "https://callback.fixture.example:9443/integrations/mcp/callback";
    advertised.authorities = names
      .slice(1, 5)
      .map((n) => `${n}:${f.config.listen.port}`);
    f.save();
    const loaded = loadConfig(f.path);
    assert.equal(loaded.options.network.origin, advertised.resource);
    assert.deepEqual(loaded.options.network.oauth, {
      issuer: advertised.issuer,
      authorization: advertised.authorization,
      token: advertised.token,
      redirectUri: advertised.redirectUri,
    });
    writeFileSync(
      f.path,
      JSON.stringify({ ...f.config, case: { reply: ["json"] } }),
    );
    assert.throws(() => loadConfig(f.path));
  } finally {
    rmSync(f.root, { recursive: true, force: true });
  }
});
