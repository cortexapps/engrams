import { createServer } from "node:https";
import type { IncomingMessage, ServerResponse } from "node:http";
import type { Tool } from "@modelcontextprotocol/client";
import { fixtureFetch } from "./http.ts";
import { createOAuthFixture } from "./oauth.ts";
import { TEST_CA, TEST_CERT, TEST_KEY } from "./tls.ts";

export type FixtureVersion = "2026-07-28" | "2025-11-25" | "2025-06-18";
export type FixtureResponse =
  | "normal"
  | "malformed"
  | "oversize"
  | "binary"
  | "image"
  | "audio"
  | "resource"
  | "resource_link"
  | "error";
export interface FixtureOptions {
  version?: FixtureVersion;
  reply?: "json" | "sse";
  response?: FixtureResponse;
  holdCalls?: boolean;
  requireAuth?: boolean;
  callbackIssuer?: string;
}
export interface ObservedRequest {
  method: string;
  sessionId: string | undefined;
  protocolVersion: string | undefined;
}
interface RpcMessage {
  jsonrpc: "2.0";
  id?: number | string;
  method: string;
  params?: Record<string, unknown>;
}

export async function startMcpFixture(options: FixtureOptions = {}) {
  const version = options.version ?? "2026-07-28";
  const modern = version === "2026-07-28";
  let origin = "";
  let revision = 1;
  let count = 0;
  let reads = 0;
  let writes = 0;
  let cancellations = 0;
  const requests: ObservedRequest[] = [];
  const sessions = new Map<string, string | undefined>();
  const pending = new Map<
    ServerResponse,
    {
      id: number | string | undefined;
      sessionId: string | undefined;
      complete: () => void;
    }
  >();
  const calls = eventBarrier();
  const canceled = eventBarrier();
  const oauth = createOAuthFixture(() => origin, options.callbackIssuer);
  const server = createServer(
    { key: TEST_KEY, cert: TEST_CERT },
    (req, res) => {
      void handle(req, res).catch((error: unknown) => {
        if (!res.headersSent) res.writeHead(500);
        res.end(
          error instanceof Error ? error.message : "Fixture request failed",
        );
      });
    },
  );
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const address = server.address();
  if (!address || typeof address === "string")
    throw new Error("Missing fixture listener");
  origin = `https://127.0.0.1:${address.port}`;

  async function handle(req: IncomingMessage, res: ServerResponse) {
    const url = new URL(req.url ?? "/", origin);
    const chunks: Buffer[] = [];
    for await (const chunk of req) chunks.push(Buffer.from(chunk));
    const raw = Buffer.concat(chunks).toString("utf8");
    if (oauth.handle(req, res, url, raw)) return;
    if (url.pathname !== "/mcp") {
      res.writeHead(404).end();
      return;
    }
    if (options.requireAuth && !oauth.accepts(req.headers.authorization)) {
      res.writeHead(401, {
        "content-type": "application/json",
        "www-authenticate": `Bearer resource_metadata="${origin}/.well-known/oauth-protected-resource"`,
      });
      res.end(JSON.stringify({ error: "unauthorized" }));
      return;
    }
    if (req.method === "GET") {
      res.writeHead(405).end();
      return;
    }
    const sessionId = header(req, "mcp-session-id");
    if (
      req.method === "DELETE" &&
      !modern &&
      sessionId &&
      sessions.has(sessionId)
    ) {
      sessions.delete(sessionId);
      res.writeHead(200).end();
      return;
    }
    if (req.method !== "POST") {
      res.writeHead(405).end();
      return;
    }
    let body: unknown;
    try {
      body = JSON.parse(raw);
    } catch {
      res.writeHead(400).end();
      return;
    }
    if (!isRpcMessage(body)) {
      res.writeHead(400).end();
      return;
    }
    const message = body;
    requests.push({
      method: message.method,
      sessionId,
      protocolVersion: header(req, "mcp-protocol-version"),
    });
    const rpcError = (code: number, text: string, status = 200) =>
      send(
        res,
        {
          jsonrpc: "2.0",
          id: message.id ?? null,
          error: { code, message: text },
        },
        status,
      );
    if (!modern && message.method === "server/discover") {
      rpcError(-32601, "Method not found");
      return;
    }
    if (!modern && message.method === "initialize") {
      const id = `fixture-session-${sessions.size + 1}`;
      sessions.set(id, req.headers.authorization);
      res.setHeader("mcp-session-id", id);
      sendResult(res, message, {
        protocolVersion: version,
        capabilities: { tools: { listChanged: true } },
        serverInfo: { name: "mcp-fixture", version: "1" },
      });
      return;
    }
    if (
      !modern &&
      (!sessionId ||
        !sessions.has(sessionId) ||
        sessions.get(sessionId) !== req.headers.authorization)
    ) {
      rpcError(-32001, "Unknown session", 404);
      return;
    }
    if (message.method === "notifications/initialized") {
      res.writeHead(202).end();
      return;
    }
    if (message.method === "notifications/cancelled") {
      for (const [response, call] of pending) {
        if (
          call.id === message.params?.requestId &&
          call.sessionId === sessionId
        )
          response.destroy();
      }
      res.writeHead(202).end();
      return;
    }
    if (modern && !validModernHeaders(req, message)) {
      rpcError(-32021, "Request metadata mismatch", 400);
      return;
    }
    switch (message.method) {
      case "server/discover":
        sendResult(res, message, {
          supportedVersions: [version],
          capabilities: { tools: {} },
          _meta: {
            "io.modelcontextprotocol/serverInfo": {
              name: "mcp-fixture",
              version: "1",
            },
          },
        });
        return;
      case "ping":
        sendResult(res, message, {});
        return;
      case "tools/list": {
        const page = message.params?.cursor;
        if (page !== undefined && page !== "page-2") {
          rpcError(-32602, "Unknown cursor");
          return;
        }
        sendResult(res, message, {
          tools: [
            catalogTool(
              page === "page-2" ? "write_counter" : "read_counter",
              revision,
            ),
          ],
          ...(page === "page-2" ? {} : { nextCursor: "page-2" }),
        });
        return;
      }
      case "tools/call": {
        const args = message.params?.arguments;
        if (!isObject(args)) {
          rpcError(-32602, "Arguments must be an object");
          return;
        }
        if (message.params?.name === "read_counter") ++reads;
        else if (
          message.params?.name === "write_counter" &&
          Number.isSafeInteger(args.delta)
        ) {
          ++writes;
          count += Number(args.delta);
        } else {
          rpcError(-32602, "Unknown tool or invalid delta");
          return;
        }
        const complete = () => toolReply(res, message, count);
        if (options.holdCalls) {
          if (options.reply === "sse") {
            res.writeHead(200, { "content-type": "text/event-stream" });
            res.flushHeaders();
          }
          pending.set(res, { id: message.id, sessionId, complete });
          res.on("close", () => {
            if (pending.delete(res) && !res.writableEnded) {
              ++cancellations;
              canceled.signal();
            }
          });
        }
        calls.signal();
        if (!options.holdCalls) complete();
        return;
      }
      default:
        rpcError(-32601, "Method not found");
    }
  }
  function send(res: ServerResponse, body: unknown, status = 200) {
    const json = JSON.stringify(body);
    if (options.reply === "sse" && status === 200) {
      if (!res.headersSent)
        res.writeHead(status, { "content-type": "text/event-stream" });
      res.end(`data: ${json}\n\n`);
    } else {
      res.writeHead(status, { "content-type": "application/json" });
      res.end(json);
    }
  }
  function sendResult(
    res: ServerResponse,
    message: RpcMessage,
    result: unknown,
  ) {
    send(res, {
      jsonrpc: "2.0",
      id: message.id,
      result:
        modern && isObject(result)
          ? {
              ...result,
              resultType: "complete",
              ...(message.method === "tools/list"
                ? { ttlMs: 0, cacheScope: "private" }
                : {}),
            }
          : result,
    });
  }
  function toolReply(res: ServerResponse, message: RpcMessage, value: number) {
    if (options.response === "binary") {
      res.writeHead(200, { "content-type": "application/octet-stream" });
      res.end(Buffer.from([0, 255, 128, 1]));
      return;
    }
    if (options.response === "malformed") {
      res.writeHead(200, { "content-type": "application/json" });
      res.end("{broken");
      return;
    }
    let content: unknown[] = [{ type: "text", text: `count=${value}` }];
    switch (options.response) {
      case "oversize":
        content = [{ type: "text", text: "x".repeat(2 * 1024 * 1024 + 1) }];
        break;
      case "image":
        content = [{ type: "image", data: "AA==", mimeType: "image/png" }];
        break;
      case "audio":
        content = [{ type: "audio", data: "AA==", mimeType: "audio/wav" }];
        break;
      case "resource":
        content = [
          {
            type: "resource",
            resource: {
              uri: "https://resource.example/data",
              text: "untrusted",
            },
          },
        ];
        break;
      case "resource_link":
        content = [
          {
            type: "resource_link",
            uri: "https://resource.example/data",
            name: "untrusted",
          },
        ];
        break;
    }
    sendResult(res, message, {
      content,
      structuredContent: { count: value },
      isError: options.response === "error",
    });
  }
  return {
    get url() {
      return `${origin}/mcp`;
    },
    get origin() {
      return origin;
    },
    ca: TEST_CA,
    fetch: fixtureFetch,
    requests,
    get reads() {
      return reads;
    },
    get writes() {
      return writes;
    },
    get cancellations() {
      return cancellations;
    },
    setCatalogRevision(value: number) {
      revision = value;
    },
    waitForCall: calls.wait,
    waitForCancellation: canceled.wait,
    releaseCalls() {
      const held = [...pending.values()];
      pending.clear();
      for (const call of held) call.complete();
    },
    async close() {
      for (const [res] of pending) res.destroy();
      pending.clear();
      server.closeAllConnections();
      await new Promise<void>((resolve, reject) =>
        server.close((error) => (error ? reject(error) : resolve())),
      );
    },
  };
}
function header(req: IncomingMessage, name: string): string | undefined {
  const value = req.headers[name];
  return typeof value === "string" ? value : undefined;
}
function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
function isRpcMessage(value: unknown): value is RpcMessage {
  return (
    isObject(value) &&
    value.jsonrpc === "2.0" &&
    typeof value.method === "string" &&
    (value.id === undefined ||
      typeof value.id === "number" ||
      typeof value.id === "string") &&
    (value.params === undefined || isObject(value.params))
  );
}
function validModernHeaders(req: IncomingMessage, message: RpcMessage) {
  const meta = message.params?._meta;
  return (
    isObject(meta) &&
    meta["io.modelcontextprotocol/protocolVersion"] === "2026-07-28" &&
    header(req, "mcp-protocol-version") === "2026-07-28" &&
    header(req, "mcp-method") === message.method &&
    (message.method !== "tools/call" ||
      header(req, "mcp-name") === message.params?.name)
  );
}
function catalogTool(name: string, revision: number): Tool {
  return {
    name,
    description: `Counter tool revision ${revision}`,
    inputSchema: {
      type: "object",
      $schema:
        name === "read_counter"
          ? "http://json-schema.org/draft-07/schema#"
          : "https://json-schema.org/draft/2020-12/schema",
      properties: {
        ...(name === "write_counter" ? { delta: { type: "integer" } } : {}),
        ...(revision > 1 ? { required_tag: { type: "string" } } : {}),
      },
      required: [
        ...(name === "write_counter" ? ["delta"] : []),
        ...(revision > 1 ? ["required_tag"] : []),
      ],
      additionalProperties: false,
    },
    annotations: {
      readOnlyHint: name === "read_counter",
      destructiveHint: name === "write_counter",
    },
  };
}
function eventBarrier() {
  let available = 0;
  const waiters: (() => void)[] = [];
  return {
    signal() {
      const waiter = waiters.shift();
      if (waiter) waiter();
      else ++available;
    },
    wait() {
      if (available > 0) {
        --available;
        return Promise.resolve();
      }
      return new Promise<void>((resolve) => waiters.push(resolve));
    },
  };
}
