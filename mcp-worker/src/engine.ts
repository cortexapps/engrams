import {
  Client,
  StreamableHTTPClientTransport,
} from "@modelcontextprotocol/client";
import type { Tool, FetchLike } from "@modelcontextprotocol/client";
import { PROTOCOL_VERSIONS } from "./contracts.ts";
import type {
  WorkerOperation,
  ExecutionResult,
  ToolOutput,
  UpstreamTool,
} from "./contracts.ts";
import { bytes, object, redact } from "./encoding.ts";
import { validateSchemas } from "./schema.ts";
import { abortable, RESPONSE_BYTES } from "./network.ts";

export async function runOperation(
  operation: WorkerOperation,
  fetch: typeof globalThis.fetch,
  signal: AbortSignal,
): Promise<unknown> {
  signal.throwIfAborted();
  if (operation.kind === "validateSchemas")
    return validateSchemas(operation.documents, operation.value, signal);
  if (operation.kind === "validateDestination")
    throw Error("destination_requires_service");
  if (operation.kind === "cancel") throw Error("cancel_requires_service");
  if (operation.kind === "metadata" || operation.kind === "token") {
    const init: RequestInit = {
      method: operation.kind === "metadata" ? "GET" : "POST",
      redirect: "error",
      signal,
      headers: { accept: "application/json" },
    };
    if (operation.kind === "token") {
      init.body = new URLSearchParams(operation.grant).toString();
      init.headers = {
        "content-type": "application/x-www-form-urlencoded",
        accept: "application/json",
      };
    }
    const response = await fetch(operation.endpoint, init);
    if (!response.ok) throw Error("upstream_rejected");
    if (!response.headers.get("content-type")?.includes("application/json"))
      throw Error("unsupported_response");
    const result: unknown = JSON.parse(await boundedText(response));
    if (!object(result)) throw Error("invalid_response");
    return result;
  }
  if (operation.kind === "execute") {
    if (bytes(operation.arguments) > 256 * 1024) throw Error("arguments_limit");
    if (
      !(await validateSchemas(
        [operation.inputSchema, ...operation.argumentConstraintDocuments],
        operation.arguments,
        signal,
      ))
    )
      throw Error("arguments_invalid");
    if (operation.outputSchema)
      await validateSchemas([operation.outputSchema], undefined, signal);
  }
  const secrets =
    "credential" in operation && operation.credential
      ? [
          operation.credential.value,
          operation.credential.value.replace(/^Bearer /, ""),
        ]
      : [];
  let verifiedOutcome: ExecutionResult["outcome"] | undefined;
  let callSent = false;
  let requests = 0;
  const scopedFetch: FetchLike = async (input, init) => {
    signal.throwIfAborted();
    const target = input instanceof Request ? input.url : String(input);
    if (target !== operation.endpoint) throw Error("destination_changed");
    if (++requests > 140) throw Error("request_limit");
    const method =
      init?.method ?? (input instanceof Request ? input.method : "GET");
    // Do not open background subscription streams or recover legacy sessions.
    if (method === "GET") return new Response(null, { status: 405 });
    if (method !== "POST") throw Error("method_denied");
    if (typeof init?.body !== "string") throw Error("request_body_invalid");
    const message: unknown = JSON.parse(init.body);
    if (
      !object(message) ||
      typeof message.method !== "string" ||
      ![
        "server/discover",
        "initialize",
        "notifications/initialized",
        "tools/list",
        "tools/call",
        "notifications/cancelled",
      ].includes(message.method)
    )
      throw Error("rpc_denied");
    if (message.method === "tools/call") {
      if (operation.kind !== "execute" || callSent)
        throw Error("repeat_call_denied");
      callSent = true;
    }
    const headers = new Headers(init.headers);
    if (operation.credential)
      headers.set(operation.credential.header, operation.credential.value);
    // The SDK does not receive the credential through an auth provider or cache.
    return fetch(target, {
      ...init,
      headers,
      redirect: "error",
      signal: AbortSignal.any([signal, ...(init.signal ? [init.signal] : [])]),
    });
  };
  const client = new Client(
    { name: "engrams-mcp-worker", version: "1" },
    {
      capabilities: {},
      supportedProtocolVersions: [...PROTOCOL_VERSIONS],
      versionNegotiation: {
        mode: "auto",
        probe: { maxRetries: 0, timeoutMs: 5000 },
      },
      inputRequired: { autoFulfill: false },
      // Low-level requests do not compile schemas. Fail closed if an SDK change
      // ever invokes its synchronous output validator instead of our child.
      jsonSchemaValidator: {
        getValidator() {
          throw Error("in_process_schema_forbidden");
        },
      },
    },
  );
  client.onerror = () => {};
  const transport = new StreamableHTTPClientTransport(
    new URL(operation.endpoint),
    {
      fetch: scopedFetch,
      requestInit: { redirect: "error" },
      reconnectionOptions: {
        maxRetries: 0,
        initialReconnectionDelay: 0,
        maxReconnectionDelay: 0,
        reconnectionDelayGrowFactor: 1,
      },
    },
  );
  const options = {
    signal,
    timeout: 300000,
    maxTotalTimeout: 300000,
    resetTimeoutOnProgress: false,
  };
  try {
    await abortable(
      client.connect(transport, { signal, timeout: 5000 }),
      signal,
    );
    const version = client.getNegotiatedProtocolVersion();
    if (!PROTOCOL_VERSIONS.some((v) => v === version))
      throw Error("protocol_denied");
    const capabilities = client.getServerCapabilities();
    if (
      !capabilities?.tools ||
      Object.keys(capabilities).some((k) => !["tools"].includes(k))
    )
      throw Error("capability_denied");
    if (operation.kind === "discover") {
      const tools: UpstreamTool[] = [];
      let cursor: string | undefined;
      const cursors = new Set<string>();
      do {
        const page = await client.request(
          { method: "tools/list", params: cursor ? { cursor } : {} },
          options,
        );
        for (const tool of page.tools)
          tools.push(discoveredTool(tool, secrets));
        if (tools.length > 128 || bytes(tools) > 1024 * 1024)
          throw Error("catalog_limit");
        cursor = page.nextCursor;
        if (cursor) {
          if (cursors.has(cursor) || cursors.size >= 128)
            throw Error("pagination_limit");
          cursors.add(cursor);
        }
      } while (cursor);
      if (new Set(tools.map((t) => t.name)).size !== tools.length)
        throw Error("duplicate_tools");
      await validateSchemas(
        tools.flatMap((t) =>
          t.outputSchema ? [t.inputSchema, t.outputSchema] : [t.inputSchema],
        ),
        undefined,
        signal,
      );
      return tools;
    }
    // Avoid callTool(): it can derive headers, compile schemas, and resend calls.
    const result = await client.request(
      {
        method: "tools/call",
        params: { name: operation.tool, arguments: operation.arguments },
      },
      options,
    );
    const outcome = result.isError ? "failed" : "succeeded";
    verifiedOutcome = outcome;
    if (result.content.some((item) => item.type !== "text"))
      return {
        outcome,
        delivery: "unavailable",
        reason: "unsupported_content",
      } satisfies ExecutionResult;
    const output: ToolOutput = {
      content: result.content.flatMap((item) =>
        item.type === "text"
          ? [{ type: "text" as const, text: redact(item.text, secrets) }]
          : [],
      ),
      ...(result.structuredContent !== undefined
        ? { structuredContent: redactJson(result.structuredContent, secrets) }
        : {}),
      ...(result.isError !== undefined ? { isError: result.isError } : {}),
    };
    if (bytes(output) > 1024 * 1024)
      return {
        outcome,
        delivery: "unavailable",
        reason: "result_limit",
      } satisfies ExecutionResult;
    if (operation.outputSchema && !result.isError) {
      try {
        if (
          result.structuredContent === undefined ||
          !(await validateSchemas(
            [operation.outputSchema],
            result.structuredContent,
            signal,
          ))
        )
          return {
            outcome,
            delivery: "unavailable",
            reason: "output_schema",
          } satisfies ExecutionResult;
      } catch {
        return {
          outcome,
          delivery: "unavailable",
          reason: "output_schema",
        } satisfies ExecutionResult;
      }
    }
    return { outcome, delivery: "available", output } satisfies ExecutionResult;
  } catch {
    if (operation.kind === "execute")
      return {
        outcome: verifiedOutcome ?? (callSent ? "outcome_unknown" : "failed"),
        delivery: "unavailable",
        reason: "upstream_failure",
      } satisfies ExecutionResult;
    throw Error("discovery_failed");
  } finally {
    await client.close().catch(() => {});
  }
}
export function discoveredTool(tool: Tool, secrets: string[]): UpstreamTool {
  if (
    containsSecret(
      {
        name: tool.name,
        inputSchema: tool.inputSchema,
        outputSchema: tool.outputSchema,
      },
      secrets,
    )
  )
    throw Error("secret_in_definition");
  if (tool.execution?.taskSupport && tool.execution.taskSupport !== "forbidden")
    throw Error("task_tool_denied");
  return {
    name: tool.name,
    ...(tool.description !== undefined
      ? { description: redact(tool.description, secrets) }
      : {}),
    inputSchema: tool.inputSchema,
    ...(tool.outputSchema ? { outputSchema: tool.outputSchema } : {}),
    ...(tool.annotations
      ? { annotations: redactJson(tool.annotations, secrets) }
      : {}),
  };
}
function redactJson<T>(value: T, secrets: string[]): T;
function redactJson(value: unknown, secrets: string[]): unknown {
  if (typeof value === "string") return redact(value, secrets);
  if (Array.isArray(value))
    return value.map((item) => redactJson(item, secrets));
  if (object(value))
    return Object.fromEntries(
      Object.entries(value).map(([key, item]) => [
        redact(key, secrets),
        redactJson(item, secrets),
      ]),
    );
  return value;
}
async function boundedText(response: Response) {
  const reader = response.body?.getReader();
  if (!reader) throw Error("empty_response");
  const chunks: Uint8Array[] = [];
  let length = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      length += value.length;
      if (length > RESPONSE_BYTES) throw Error("response_limit");
      chunks.push(value);
    }
    return Buffer.concat(chunks).toString("utf8");
  } finally {
    await reader.cancel().catch(() => {});
  }
}

function containsSecret(value: unknown, secrets: string[]): boolean {
  if (typeof value === "string")
    return secrets.some(
      (secret) => secret.length > 0 && value.includes(secret),
    );
  if (Array.isArray(value))
    return value.some((item) => containsSecret(item, secrets));
  if (object(value))
    return Object.entries(value).some(
      ([key, item]) =>
        containsSecret(key, secrets) || containsSecret(item, secrets),
    );
  return false;
}
