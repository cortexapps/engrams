import { request as httpsRequest } from "node:https";
import { checkServerIdentity, type ConnectionOptions } from "node:tls";
import { ConnectError, createClient, type Interceptor } from "@connectrpc/connect";
import {
  createConnectTransport,
  createGrpcTransport,
} from "@connectrpc/connect-node";
import {
  AutomationService,
  AutomationRunService,
} from "../../src/gen/engram/app/v1/automation_pb.ts";
import { ApiKeyService } from "../../src/gen/engram/app/v1/api_key_pb.ts";
import { IntegrationService } from "../../src/gen/engram/app/v1/integration_pb.ts";
import { ProfileService } from "../../src/gen/engram/app/v1/profile_pb.ts";
import { TaskService } from "../../src/gen/engram/app/v1/task_pb.ts";
import { SessionService } from "../../src/gen/engram/app/v1/session_pb.ts";
import { OrgSecretService } from "../../src/gen/engram/app/v1/org_secret_pb.ts";
import { ModelRouterService } from "../../src/gen/engram/app/v1/model_router_pb.ts";
import { FleetService } from "../../src/gen/engram/app/v1/fleet_pb.ts";
import { ImageService } from "../../src/gen/engram/app/v1/image_pb.ts";
import { OperatorPublicationService } from "../../src/gen/engram/mcp/v1/operator_pb.ts";
import {
  privateFile,
  requireThat,
  sha256,
  RunnerFailure,
  type Context,
  type Endpoint,
} from "./context.ts";

const publicRpcMethods = new WeakMap<ConnectError, string>();
export function publicRpcMethod(error: ConnectError): string | undefined {
  return publicRpcMethods.get(error);
}

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
    for (const [key, value] of Object.entries(headers)) req.header.set(key, value);
    req.header.set("origin", ctx.endpoints.app.origin);
    const method = `${req.service.name}.${req.method.name}`;
    const remember = (error: unknown) => {
      if (error instanceof ConnectError && /^[A-Za-z0-9_]{1,80}\.[A-Za-z0-9_]{1,80}$/.test(method))
        publicRpcMethods.set(error, method);
    };
    try {
      const response = await next(req);
      if (!response.stream) return response;
      const message = response.message;
      return {
        ...response,
        message: (async function* () {
          try {
            yield* message;
          } catch (error) {
            remember(error);
            throw error;
          }
        })(),
      };
    } catch (error) {
      remember(error);
      throw error;
    }
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
    downloadGuestProofFile: (sessionId: string, path: string) =>
      downloadGuestProofFile(ctx, headers, sessionId, path),
    identity: createClient(ApiKeyService, transport),
    automations: createClient(AutomationService, transport),
    automationRuns: createClient(AutomationRunService, transport),
    integration: createClient(IntegrationService, transport),
    profiles: createClient(ProfileService, transport),
    tasks: createClient(TaskService, transport),
    sessions: createClient(SessionService, transport),
    secrets: createClient(OrgSecretService, transport),
    models: createClient(ModelRouterService, transport),
    fleet: createClient(FleetService, transport),
    images: createClient(ImageService, transport),
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

/** Read the owner-gated file route with the same credentials and TLS pin as RPC. */
async function downloadGuestProofFile(
  ctx: Context,
  headers: Record<string, string>,
  sessionId: string,
  path: string,
): Promise<Buffer> {
  const tls = await tlsOptions(ctx, ctx.endpoints.app);
  const route = `/api/v1/sessions/${encodeURIComponent(sessionId)}/files?path=${encodeURIComponent(path)}`;
  return await new Promise<Buffer>((resolve, reject) => {
    const fail = (detail = "guest_file_download") => {
      clearTimeout(timer);
      reject(new RunnerFailure("TRANSPORT_FAILED", detail));
      req.destroy();
    };
    const req = httpsRequest(
      new URL(route, ctx.endpoints.app.origin),
      {
        ...tls,
        method: "GET",
        agent: false,
        headers: { ...headers, origin: ctx.endpoints.app.origin },
      },
      (res) => {
        if (res.statusCode !== 200) {
          const status = res.statusCode;
          if (!status || !Number.isInteger(status) || status < 100 || status > 599)
            return fail("guest_file_status");
          const rpcCode = res.headers["x-engram-rpc-code"];
          return fail(
            `guest_file_status_http_${status}` +
              (typeof rpcCode === "string" && /^(?:[1-9]|1[0-6])$/.test(rpcCode)
                ? `_rpc_code_${Number(rpcCode)}`
                : ""),
          );
        }
        const size = res.headers["content-length"];
        const hash = res.headers["x-content-sha256"];
        if (
          typeof size !== "string" ||
          !/^[0-9]+$/.test(size) ||
          BigInt(size) > 1024n * 1024n ||
          typeof hash !== "string" ||
          !/^[a-f0-9]{64}$/.test(hash)
        )
          return fail("guest_file_metadata");
        const chunks: Buffer[] = [];
        let bytes = 0;
        res.on("data", (chunk: Buffer) => {
          bytes += chunk.length;
          if (bytes > 1024 * 1024) fail("guest_proof_size");
          else chunks.push(chunk);
        });
        res.on("error", () => fail());
        res.on("aborted", () => fail());
        res.on("end", () => {
          clearTimeout(timer);
          const value = Buffer.concat(chunks);
          try {
            requireThat(
              BigInt(value.length) === BigInt(size) && sha256(value) === hash,
              "guest_file_integrity",
            );
            resolve(value);
          } catch (error) {
            reject(error);
          }
        });
      },
    );
    const timer = setTimeout(() => fail(), ctx.deadlines.requestMs);
    req.on("error", () => fail());
    req.end();
  });
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
