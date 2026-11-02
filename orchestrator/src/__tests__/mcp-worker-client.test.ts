import { expect, test } from "bun:test";
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { createServer } from "node:https";
import type { IncomingMessage, ServerResponse } from "node:http";
import { once } from "node:events";
import { workerTls } from "../../../mcp-worker/test/fixtures/worker-tls.ts";
import { digest } from "../../../mcp-worker/src/index.ts";
import type {
  WorkerIdentity,
  WorkerOperation,
  WorkerRequest,
} from "../../../mcp-worker/src/contracts.ts";
import { makeResourceWorkerClient } from "../mcp/worker-client.ts";
import { toWorkerBindingHash } from "../mcp/worker-encoding.ts";

function envelope(identity: WorkerIdentity, operation: WorkerOperation): WorkerRequest {
  const expiresAt = Date.now() + 10000;
  return {
    protocol: 1,
    intent: {
      id: crypto.randomUUID(),
      bootId: identity.bootId,
      zoneId: identity.zoneId,
      policyDigest: identity.policyDigest,
      connectionId: "connection",
      connectionRevision: 1,
      credentialIdentity: "credential",
      operation: operation.kind,
      scope: {
        kind: "execution",
        sessionId: "session",
        runId: "run",
        toolCallId: "call",
        bindingHash: toWorkerBindingHash(`sha256:${"b".repeat(64)}`),
      },
      destination: "endpoint" in operation ? operation.endpoint : "",
      requestDigest: `sha256:${"c".repeat(64)}`,
      operationDigest: digest(operation),
      expiresAt,
      lease: { id: crypto.randomUUID(), deploymentId: "fixture-deployment", expiresAt },
    },
    operation,
  };
}
const schema: WorkerOperation = {
  kind: "validateSchemas",
  documents: [{ type: "integer" }],
  value: 1,
};
function options(
  tls: Awaited<ReturnType<typeof workerTls>>,
  port: number,
  identity: WorkerIdentity,
) {
  return {
    tls: { ...tls, url: `https://127.0.0.1:${port}`, serverFingerprint: tls.fingerprint },
    policy: {
      zoneId: identity.zoneId,
      policyRevision: identity.policyRevision,
      policyDigest: `sha256:${identity.policyDigest}`,
    },
  };
}
async function actualWorker() {
  const tls = await workerTls("localhost");
  const process = spawn(
    globalThis.process.env.MCP_WORKER_TEST_NODE ?? "node",
    [
      new URL("../../../mcp-worker/test/fixtures/resource-client-worker.ts", import.meta.url)
        .pathname,
    ],
    { stdio: ["pipe", "pipe", "pipe"], env: { PATH: globalThis.process.env.PATH ?? "" } },
  );
  const output = createInterface({ input: process.stdout });
  const lines = output[Symbol.asyncIterator]();
  let stderr = "";
  process.stderr.on("data", (data) => {
    stderr += String(data);
  });
  process.stdin.write(`${JSON.stringify(tls)}\n`);
  async function next() {
    const line = await lines.next();
    if (line.done) throw new Error(`worker fixture exited: ${stderr}`);
    return JSON.parse(line.value);
  }
  const ready: { port: number; identity: WorkerIdentity; node: string } = await next();
  expect(ready.node.startsWith("24.")).toBe(true);
  return {
    ...ready,
    tls,
    async command(command: string) {
      process.stdin.write(`${command}\n`);
      return next();
    },
    async close() {
      const exited = once(process, "exit");
      process.stdin.end("close\n");
      await exited;
      output.close();
      expect(stderr).toBe("");
    },
  };
}

async function protocolFixture(
  reply: (req: IncomingMessage, res: ServerResponse, body: WorkerRequest | undefined) => void,
) {
  const tls = await workerTls("localhost");
  const identity: WorkerIdentity = {
    protocol: 1,
    bootId: "boot",
    zoneId: "public",
    policyRevision: "1",
    policyDigest: "a".repeat(64),
  };
  let rpcs = 0,
    identities = 0,
    bytes = 0;
  const sockets = new Set<unknown>();
  const server = createServer(
    { ...tls, requestCert: true, rejectUnauthorized: true },
    (req, res) => {
      sockets.add(req.socket);
      if (req.url === "/identity") {
        identities++;
        res.writeHead(200, { "content-type": "application/json" });
        res.end(JSON.stringify(identity));
      } else {
        rpcs++;
        let text = "";
        req.on("data", (chunk: Buffer) => {
          bytes += chunk.length;
          text += chunk;
        });
        req.on("end", () => reply(req, res, text ? JSON.parse(text) : undefined));
      }
    },
  );
  server.maxRequestsPerSocket = 2;
  server.keepAliveTimeout = 35000;
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("test address");
  return {
    identity,
    client: (signal?: AbortSignal) =>
      makeResourceWorkerClient({ ...options(tls, address.port, identity), signal }),
    counts: () => ({ rpcs, identities, bytes, sockets: sockets.size }),
    async close() {
      server.closeAllConnections();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}
function success(identity: WorkerIdentity, request: WorkerRequest) {
  return {
    ok: true,
    identity,
    intent: request.intent,
    intentId: request.intent.id,
    requestDigest: request.intent.requestDigest,
    operationDigest: request.intent.operationDigest,
    leaseId: request.intent.lease.id,
    result: true,
  };
}

test("actual worker accepts execution-scoped disposable schemas on the pinned handshake channel", async () => {
  const fixture = await actualWorker();
  const client = makeResourceWorkerClient(options(fixture.tls, fixture.port, fixture.identity));
  try {
    expect(await client.identity()).toEqual(fixture.identity);
    // The broker repeats the real identity check before acquiring a credential.
    expect(await client.identity()).toEqual(fixture.identity);
    const request = envelope(fixture.identity, schema);
    const response = await client.dispatch(request);
    expect(response.ok && response.result).toBe(true);
    expect(response.ok && response.intent).toEqual(request.intent);
    await expect(client.dispatch(request)).rejects.toThrow("MCP worker");
    expect((await fixture.command("stats")).providerDials).toBe(0);
  } finally {
    client.close();
    await fixture.close();
  }
}, 15000);

test("a closed actual worker channel cannot send credentials to a replacement process", async () => {
  const fixture = await actualWorker();
  const client = makeResourceWorkerClient(options(fixture.tls, fixture.port, fixture.identity));
  try {
    await client.identity();
    await fixture.command("replace");
    const operation: WorkerOperation = {
      kind: "execute",
      endpoint: "https://tools.example/mcp",
      tool: "write",
      arguments: {},
      inputSchema: {},
      argumentConstraintDocuments: [],
      classification: "write",
      credential: { header: "authorization", value: "Bearer transient-provider-value" },
    };
    await expect(client.dispatch(envelope(fixture.identity, operation))).rejects.toThrow(
      "MCP worker",
    );
    expect(await fixture.command("stats")).toEqual({
      providerDials: 0,
      replacementBytes: 0,
      replacementRequests: 0,
    });
  } finally {
    client.close();
    await fixture.close();
  }
}, 15000);

test("an actual worker restart changes boot and requires a new operation client", async () => {
  const fixture = await actualWorker();
  const oldClient = makeResourceWorkerClient(options(fixture.tls, fixture.port, fixture.identity));
  const nextClient = makeResourceWorkerClient(options(fixture.tls, fixture.port, fixture.identity));
  try {
    const oldIdentity = await oldClient.identity();
    const restarted: { identity: WorkerIdentity } = await fixture.command("restart");
    expect(restarted.identity.bootId).not.toBe(oldIdentity.bootId);
    await expect(oldClient.dispatch(envelope(oldIdentity, schema))).rejects.toThrow("MCP worker");
    expect(await nextClient.identity()).toEqual(restarted.identity);
    const response = await nextClient.dispatch(envelope(restarted.identity, schema));
    expect(response.ok && response.result).toBe(true);
    expect((await fixture.command("stats")).providerDials).toBe(0);
  } finally {
    oldClient.close();
    nextClient.close();
    await fixture.close();
  }
}, 15000);

test("actual schema children keep independent roots, reject coercion and do not dial providers", async () => {
  const fixture = await actualWorker();
  try {
    const documents = [
      {
        $schema: "http://json-schema.org/draft-07/schema#",
        $id: "https://schema.example/constraint",
        definitions: { value: { type: "integer", maximum: 5 } },
        properties: { n: { $ref: "#/definitions/value" } },
        required: ["n"],
      },
      {
        $schema: "https://json-schema.org/draft/2020-12/schema",
        $id: "https://schema.example/constraint",
        $defs: { value: { type: "integer", minimum: 2 } },
        properties: { n: { $ref: "#/$defs/value" } },
      },
    ];
    for (const [value, expected] of [
      [{ n: 3 }, true],
      [{ n: 1 }, false],
      [{ n: 6 }, false],
      [{ n: "3" }, false],
    ] as const) {
      const client = makeResourceWorkerClient(options(fixture.tls, fixture.port, fixture.identity));
      try {
        const identity = await client.identity();
        const response = await client.dispatch(
          envelope(identity, { kind: "validateSchemas", documents, value }),
        );
        expect(response.ok && response.result).toBe(expected);
      } finally {
        client.close();
      }
    }
    expect((await fixture.command("stats")).providerDials).toBe(0);
  } finally {
    await fixture.close();
  }
}, 15000);

test("separate operation clients have separate channels and never retry or reuse an RPC", async () => {
  const fixture = await protocolFixture((_req, res, request) => {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify(success(fixture.identity, request!)));
  });
  const clients = [fixture.client(), fixture.client()];
  try {
    const identities = await Promise.all(clients.map((client) => client.identity()));
    await Promise.all(
      clients.map(async (client, index) => {
        const request = envelope(identities[index]!, schema);
        expect((await client.dispatch(request)).ok).toBe(true);
        await expect(client.dispatch(request)).rejects.toThrow("MCP worker");
        await expect(client.identity()).rejects.toThrow("MCP worker");
      }),
    );
    expect(fixture.counts().rpcs).toBe(2);
    expect(fixture.counts().identities).toBe(2);
    expect(fixture.counts().sockets).toBe(2);
  } finally {
    clients.forEach((client) => client.close());
    await fixture.close();
  }
});

test("response identity and full intent mismatches fail with sanitized errors and one send", async () => {
  const changes = [
    (response: ReturnType<typeof success>) => ({
      ...response,
      identity: { ...response.identity, bootId: "new-boot" },
    }),
    (response: ReturnType<typeof success>) => ({
      ...response,
      identity: { ...response.identity, policyRevision: "2" },
    }),
    (response: ReturnType<typeof success>) => ({
      ...response,
      intent: { ...response.intent, scope: { ...response.intent.scope, runId: "other" } },
    }),
    (response: ReturnType<typeof success>) => ({
      ...response,
      intent: {
        ...response.intent,
        lease: { ...response.intent.lease, expiresAt: response.intent.lease.expiresAt + 1 },
      },
    }),
    (response: ReturnType<typeof success>) => ({ ...response, intentId: "other" }),
    (response: ReturnType<typeof success>) => ({ ...response, operationDigest: "other" }),
    (response: ReturnType<typeof success>) => ({ ...response, requestDigest: "other" }),
    (response: ReturnType<typeof success>) => ({ ...response, leaseId: "other" }),
    () => ({ ok: false, error: "Bearer private-value with upstream cause" }),
  ];
  for (const change of changes) {
    const fixture = await protocolFixture((_req, res, request) => {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify(change(success(fixture.identity, request!))));
    });
    const client = fixture.client();
    try {
      await client.identity();
      await expect(client.dispatch(envelope(fixture.identity, schema))).rejects.toThrow(
        /^MCP worker operation unavailable$/,
      );
      expect(fixture.counts().rpcs).toBe(1);
    } finally {
      client.close();
      await fixture.close();
    }
  }
}, 15000);

test("redirect, oversized, truncated and invalid UTF-8 replies cannot trigger a retry", async () => {
  for (const kind of ["redirect", "size", "truncated", "utf8"] as const) {
    const fixture = await protocolFixture((_req, res) => {
      if (kind === "redirect") {
        res.writeHead(307, { location: "/identity" });
        res.end();
      } else if (kind === "size") {
        res.writeHead(200, { "content-type": "application/json" });
        res.end("x".repeat(2 * 1024 * 1024 + 1));
      } else if (kind === "truncated") {
        res.writeHead(200, { "content-type": "application/json", "content-length": 100 });
        res.end("{");
      } else {
        res.writeHead(200, { "content-type": "application/json" });
        res.end(Buffer.from([0x22, 0xff, 0x22]));
      }
    });
    const client = fixture.client();
    try {
      await client.identity();
      const request = envelope(fixture.identity, schema);
      request.intent.expiresAt = Date.now() + 100;
      request.intent.lease.expiresAt = request.intent.expiresAt;
      await expect(client.dispatch(request)).rejects.toThrow(/^MCP worker operation unavailable$/);
      expect(fixture.counts().rpcs).toBe(1);
      expect(fixture.counts().identities).toBe(1);
    } finally {
      client.close();
      await fixture.close();
    }
  }
}, 15000);

test("abort ends an active RPC and concurrent calls cannot replace its channel", async () => {
  let entered!: () => void;
  const received = new Promise<void>((resolve) => {
    entered = resolve;
  });
  const fixture = await protocolFixture(() => {
    entered();
  });
  const controller = new AbortController(),
    client = fixture.client(controller.signal);
  try {
    await client.identity();
    const request = envelope(fixture.identity, schema);
    const pending = client.dispatch(request);
    await received;
    await expect(client.identity()).rejects.toThrow("MCP worker");
    await expect(client.dispatch(request)).rejects.toThrow("MCP worker");
    controller.abort();
    await expect(pending).rejects.toThrow(/^MCP worker operation unavailable$/);
    expect(fixture.counts().rpcs).toBe(1);
  } finally {
    client.close();
    await fixture.close();
  }
});

test("changed boot, expired lease, invalid operation digest and oversized request send zero RPC bodies", async () => {
  const fixture = await protocolFixture(() => {
    throw new Error("unexpected RPC");
  });
  try {
    for (const kind of ["boot", "lease", "digest", "size", "role"] as const) {
      const client = fixture.client();
      try {
        await client.identity();
        const request = envelope(fixture.identity, schema);
        if (kind === "boot") request.intent.bootId = "other";
        else if (kind === "lease") request.intent.lease.expiresAt = Date.now() - 1;
        else if (kind === "digest") request.intent.operationDigest = "wrong";
        else if (kind === "role") {
          request.operation = { kind: "metadata", endpoint: "https://tools.example/metadata" };
          request.intent.operation = "metadata";
          request.intent.destination = request.operation.endpoint;
          request.intent.scope = { kind: "administration", requestId: "request", actorId: "actor" };
          request.intent.operationDigest = digest(request.operation);
        } else {
          request.operation = {
            kind: "validateSchemas",
            documents: [{ description: "x".repeat(1536 * 1024) }],
          };
          request.intent.operationDigest = digest(request.operation);
        }
        await expect(client.dispatch(request)).rejects.toThrow("MCP worker");
      } finally {
        client.close();
      }
    }
    expect(fixture.counts().bytes).toBe(0);
    expect(fixture.counts().rpcs).toBe(0);
  } finally {
    await fixture.close();
  }
});

test("TLS trust, caller identity, server pin and zone policy fail before any RPC", async () => {
  const tls = await workerTls("localhost"),
    other = await workerTls("localhost");
  const identity: WorkerIdentity = {
    protocol: 1,
    bootId: "boot",
    zoneId: "public",
    policyRevision: "1",
    policyDigest: "a".repeat(64),
  };
  let rpcs = 0;
  const server = createServer(
    { ...tls, requestCert: true, rejectUnauthorized: true },
    (req, res) => {
      if (req.url !== "/identity") rpcs++;
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify(identity));
    },
  );
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("test address");
  const good = options(tls, address.port, identity);
  try {
    for (const bad of [
      { ...good, tls: { ...good.tls, ca: other.ca } },
      { ...good, tls: { ...good.tls, cert: other.cert, key: other.key } },
      { ...good, tls: { ...good.tls, serverFingerprint: other.fingerprint } },
      { ...good, policy: { ...good.policy, zoneId: "other" } },
      { ...good, policy: { ...good.policy, policyRevision: "2" } },
      { ...good, policy: { ...good.policy, policyDigest: `sha256:${"b".repeat(64)}` } },
    ]) {
      const client = makeResourceWorkerClient(bad);
      try {
        await expect(client.identity()).rejects.toThrow("MCP worker");
      } finally {
        client.close();
      }
    }
    expect(() => makeResourceWorkerClient({ ...good, tls: { ...good.tls, key: "" } })).toThrow();
    expect(() =>
      makeResourceWorkerClient({
        ...good,
        tls: { ...good.tls, url: good.tls.url.replace("https:", "http:") },
      }),
    ).toThrow();
    for (const url of [
      "not a URL",
      `${good.tls.url}/proxy`,
      `${good.tls.url}?target=other`,
      good.tls.url.replace("https://", "https://user:password@"),
    ]) {
      expect(() => makeResourceWorkerClient({ ...good, tls: { ...good.tls, url } })).toThrow(
        /^MCP worker requires pinned mTLS and policy$/,
      );
    }
    expect(rpcs).toBe(0);
  } finally {
    server.closeAllConnections();
    await new Promise<void>((resolve) => server.close(() => resolve()));
  }
}, 15000);

test("server certificate pin cannot replace hostname verification", async () => {
  const tls = await workerTls(); // Valid, pinned certificate for tools.example, not 127.0.0.1.
  const identity: WorkerIdentity = {
    protocol: 1,
    bootId: "boot",
    zoneId: "public",
    policyRevision: "1",
    policyDigest: "a".repeat(64),
  };
  let requests = 0;
  const server = createServer(
    { ...tls, requestCert: true, rejectUnauthorized: true },
    (_req, res) => {
      requests++;
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify(identity));
    },
  );
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("test address");
  const client = makeResourceWorkerClient(options(tls, address.port, identity));
  try {
    await expect(client.identity()).rejects.toThrow("MCP worker");
    expect(requests).toBe(0);
  } finally {
    client.close();
    server.closeAllConnections();
    await new Promise<void>((resolve) => server.close(() => resolve()));
  }
});
