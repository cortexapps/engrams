import { request as httpsRequest } from "node:https";
import { checkServerIdentity, type ConnectionOptions } from "node:tls";
import { createClient, type Interceptor } from "@connectrpc/connect";
import {
  createConnectTransport,
  createGrpcTransport,
} from "@connectrpc/connect-node";
import { ApiKeyService } from "../../src/gen/engram/app/v1/api_key_pb.ts";
import { IntegrationService } from "../../src/gen/engram/app/v1/integration_pb.ts";
import { ProfileService } from "../../src/gen/engram/app/v1/profile_pb.ts";
import { TaskService } from "../../src/gen/engram/app/v1/task_pb.ts";
import { SessionService } from "../../src/gen/engram/app/v1/session_pb.ts";
import { OrgSecretService } from "../../src/gen/engram/app/v1/org_secret_pb.ts";
import { ModelRouterService } from "../../src/gen/engram/app/v1/model_router_pb.ts";
import { OperatorPublicationService } from "../../src/gen/engram/mcp/v1/operator_pb.ts";
import {
  privateFile,
  RunnerFailure,
  type Context,
  type Endpoint,
} from "./context.ts";

export type Credential =
  | { kind: "cookie"; value: string }
  | { kind: "api-key"; value: string };
export function credentialHeaders(
  credential: Credential,
): Record<string, string> {
  if (!credential.value || /[\r\n]/.test(credential.value))
    throw new RunnerFailure("INVALID_CONTEXT", "credential_shape");
  if (
    credential.kind === "api-key" &&
    !/^engk_[A-Za-z0-9_-]+$/.test(credential.value)
  )
    throw new RunnerFailure("INVALID_CONTEXT", "api_key_shape");
  return credential.kind === "cookie"
    ? { cookie: credential.value }
    : { "x-api-key": credential.value };
}
export async function tlsOptions(
  ctx: Context,
  endpoint: Endpoint,
): Promise<ConnectionOptions> {
  return {
    ca: await privateFile(ctx.runRoot, endpoint.ca.path),
    rejectUnauthorized: true,
    checkServerIdentity(host, cert) {
      const error = checkServerIdentity(host, cert);
      return (
        error ??
        (cert.fingerprint256 === endpoint.serverFingerprint
          ? undefined
          : new Error("fixture_certificate_pin"))
      );
    },
  };
}
export async function publicClient(ctx: Context, credential: Credential) {
  const headers = credentialHeaders(credential);
  const authenticate: Interceptor = (next) => async (req) => {
    for (const [key, value] of Object.entries(headers))
      req.header.set(key, value);
    req.header.set("origin", ctx.endpoints.app.origin);
    return next(req);
  };
  const transport = createConnectTransport({
    baseUrl: ctx.endpoints.app.origin + "/rpc",
    httpVersion: "1.1",
    nodeOptions: {
      ...(await tlsOptions(ctx, ctx.endpoints.app)),
      agent: false,
    },
    interceptors: [authenticate],
    defaultTimeoutMs: ctx.deadlines.requestMs,
    readMaxBytes: 4 * 1024 * 1024,
    writeMaxBytes: 1024 * 1024,
    acceptCompression: [],
    useBinaryFormat: true,
  });
  return {
    identity: createClient(ApiKeyService, transport),
    integration: createClient(IntegrationService, transport),
    profiles: createClient(ProfileService, transport),
    tasks: createClient(TaskService, transport),
    sessions: createClient(SessionService, transport),
    secrets: createClient(OrgSecretService, transport),
    models: createClient(ModelRouterService, transport),
  };
}
export type PublicClient = Awaited<ReturnType<typeof publicClient>>;
export async function operatorClient(ctx: Context) {
  const ep = ctx.endpoints.operator;
  return createClient(
    OperatorPublicationService,
    createGrpcTransport({
      baseUrl: ep.origin,
      nodeOptions: {
        ...(await tlsOptions(ctx, ep)),
        cert: await privateFile(ctx.runRoot, ep.certificate.path),
        key: await privateFile(ctx.runRoot, ep.privateKeyFile),
      },
      defaultTimeoutMs: ctx.deadlines.requestMs,
      readMaxBytes: 1024 * 1024,
      writeMaxBytes: 65536,
      acceptCompression: [],
      idleConnectionTimeoutMs: 1000,
    }),
  );
}

/** One bounded HTTPS request. Never follows a redirect or logs response bodies. */
export async function jsonRequest(
  ctx: Context,
  ep: Endpoint,
  method: "GET" | "POST",
  route: string,
  body?: unknown,
  headers: Record<string, string> = {},
) {
  if (!route.startsWith("/") || route.startsWith("//") || /[\r\n#]/.test(route))
    throw new RunnerFailure("INVALID_CONTEXT", "request_route");
  const payload =
    body === undefined ? undefined : Buffer.from(JSON.stringify(body));
  if (payload && payload.length > 65536)
    throw new RunnerFailure("INVALID_CONTEXT", "request_size");
  const tls = await tlsOptions(ctx, ep);
  return await new Promise<{
    status: number;
    headers: import("node:http").IncomingHttpHeaders;
    body: unknown;
  }>((resolve, reject) => {
    const fail = () =>
      reject(new RunnerFailure("TRANSPORT_FAILED", "https_request"));
    const req = httpsRequest(
      new URL(route, ep.origin),
      {
        ...tls,
        method,
        agent: false,
        headers: {
          ...headers,
          ...(payload
            ? {
                "content-type": "application/json",
                "content-length": String(payload.length),
              }
            : {}),
        },
      },
      (res) => {
        const chunks: Buffer[] = [];
        let bytes = 0;
        res.on("data", (chunk: Buffer) => {
          bytes += chunk.length;
          if (bytes > 1024 * 1024) req.destroy();
          else chunks.push(chunk);
        });
        res.on("error", fail);
        res.on("end", () => {
          clearTimeout(timer);
          try {
            resolve({
              status: res.statusCode ?? 0,
              headers: res.headers,
              body: bytes
                ? JSON.parse(Buffer.concat(chunks).toString("utf8"))
                : null,
            });
          } catch {
            fail();
          }
        });
      },
    );
    const timer = setTimeout(() => req.destroy(), ctx.deadlines.requestMs);
    req.on("error", () => {
      clearTimeout(timer);
      fail();
    });
    req.end(payload);
  });
}
