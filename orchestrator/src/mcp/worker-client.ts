import { Agent, request as httpsRequest } from "node:https";
import { checkServerIdentity, TLSSocket } from "node:tls";
import type { IncomingMessage } from "node:http";
import type {
  WorkerIdentity,
  WorkerRequest,
  WorkerResponse,
} from "../../../mcp-worker/src/contracts.ts";
import { canonical, digest, parseWorkerRequest } from "../../../mcp-worker/src/index.ts";
import { assertWorkerPolicy, type ExpectedWorkerPolicy } from "./worker-encoding.ts";
import type { CredentialControlTls } from "./control-client.ts";

const REQUEST_BYTES = 1536 * 1024;
const RESPONSE_BYTES = 2 * 1024 * 1024;
const IDENTITY_BYTES = 4096;
const CHANNEL_MS = 35000;
const failure = () => new Error("MCP worker operation unavailable");
function object(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
function identityFrom(value: unknown): WorkerIdentity {
  if (
    !object(value) ||
    value.protocol !== 1 ||
    typeof value.bootId !== "string" ||
    !value.bootId ||
    value.bootId.length > 8192 ||
    typeof value.zoneId !== "string" ||
    !value.zoneId ||
    typeof value.policyRevision !== "string" ||
    !value.policyRevision ||
    typeof value.policyDigest !== "string" ||
    !/^[0-9a-f]{64}$/.test(value.policyDigest)
  )
    throw failure();
  return {
    protocol: 1,
    bootId: value.bootId,
    zoneId: value.zoneId,
    policyRevision: value.policyRevision,
    policyDigest: value.policyDigest,
  };
}

/** Create one client per operation. Never share this mutable channel across calls.
 * An unused identity check can be replaced before credential acquisition. Dispatch is
 * one-use, and cannot reconnect or move the credential body to another TLS socket. */
export function makeResourceWorkerClient(options: {
  tls: CredentialControlTls;
  policy: ExpectedWorkerPolicy;
  signal?: AbortSignal;
}) {
  const tls = { ...options.tls },
    policy = { ...options.policy };
  const operationSignal = options.signal;
  let url: URL;
  try {
    url = new URL(tls.url);
  } catch {
    throw new Error("MCP worker requires pinned mTLS and policy");
  }
  if (
    url.protocol !== "https:" ||
    url.username ||
    url.password ||
    url.search ||
    url.hash ||
    url.pathname !== "/" ||
    !tls.ca ||
    !tls.cert ||
    !tls.key ||
    !/^(?:[0-9A-F]{2}:){31}[0-9A-F]{2}$/.test(tls.serverFingerprint) ||
    !policy.zoneId ||
    !policy.policyRevision ||
    !/^sha256:[0-9a-f]{64}$/.test(policy.policyDigest)
  )
    throw new Error("MCP worker requires pinned mTLS and policy");

  let agent: Agent | undefined;
  let channel: { socket: TLSSocket; identity: WorkerIdentity } | undefined;
  let idleTimer: ReturnType<typeof setTimeout> | undefined;
  let state: "unused" | "handshake" | "ready" | "dispatch" | "closed" = "unused";
  function discardChannel() {
    clearTimeout(idleTimer);
    channel = undefined;
    agent?.destroy();
    agent = undefined;
  }
  function close() {
    state = "closed";
    discardChannel();
  }

  async function exchange(
    path: "/identity" | "/rpc",
    body: string | undefined,
    maximumBytes: number,
    deadline: number,
    signal?: AbortSignal,
    requiredSocket?: TLSSocket,
  ): Promise<{ value: unknown; socket: TLSSocket }> {
    if (!agent || signal?.aborted || deadline <= Date.now()) throw failure();
    return new Promise((resolve, reject) => {
      let finished = false;
      let response: IncomingMessage | undefined;
      const req = httpsRequest(url, {
        agent,
        path,
        method: body === undefined ? "GET" : "POST",
        headers: {
          accept: "application/json",
          ...(body === undefined
            ? {}
            : {
                "content-type": "application/json",
                "content-length": Buffer.byteLength(body),
              }),
        },
        maxHeaderSize: 8192,
      });
      const fail = () => {
        if (finished) return;
        finished = true;
        cleanup();
        response?.destroy();
        req.destroy();
        reject(failure());
      };
      const timer = setTimeout(fail, Math.max(1, deadline - Date.now()));
      const connectTimer = setTimeout(fail, Math.min(5000, Math.max(1, deadline - Date.now())));
      function cleanup() {
        clearTimeout(timer);
        clearTimeout(connectTimer);
        signal?.removeEventListener("abort", fail);
      }
      signal?.addEventListener("abort", fail, { once: true });
      req.once("error", fail);
      req.once("socket", (socket) => {
        if (
          !(socket instanceof TLSSocket) ||
          (requiredSocket && (socket !== requiredSocket || socket.destroyed || !socket.authorized))
        ) {
          fail();
          return;
        }
        if (requiredSocket) {
          clearTimeout(connectTimer);
          // No credential bytes are written until socket identity has been checked.
          req.end(body);
        } else {
          socket.once("secureConnect", () => {
            if (
              !socket.authorized ||
              socket.getPeerCertificate().fingerprint256 !== tls.serverFingerprint
            ) {
              fail();
              return;
            }
            clearTimeout(connectTimer);
            req.end();
          });
        }
      });
      req.once("response", (res) => {
        response = res;
        const socket = res.socket;
        if (
          !(socket instanceof TLSSocket) ||
          res.statusCode !== 200 ||
          !/^application\/json(?:\s*;|$)/i.test(res.headers["content-type"] ?? "") ||
          (res.headers["content-encoding"] && res.headers["content-encoding"] !== "identity") ||
          Number(res.headers["content-length"] ?? 0) > maximumBytes
        ) {
          fail();
          return;
        }
        const chunks: Buffer[] = [];
        let size = 0;
        res.on("data", (chunk: Buffer) => {
          size += chunk.length;
          if (size > maximumBytes) {
            fail();
            return;
          }
          chunks.push(chunk);
        });
        res.once("error", fail);
        res.once("aborted", fail);
        res.once("end", () => {
          if (finished) return;
          try {
            const value: unknown = JSON.parse(
              new TextDecoder("utf-8", { fatal: true }).decode(Buffer.concat(chunks)),
            );
            finished = true;
            cleanup();
            resolve({ value, socket });
          } catch {
            fail();
          }
        });
      });
      if (signal?.aborted) fail();
    });
  }
  return {
    async identity(): Promise<WorkerIdentity> {
      if (state !== "unused" && state !== "ready") throw failure();
      discardChannel();
      state = "handshake";
      try {
        agent = new Agent({
          keepAlive: true,
          maxSockets: 1,
          maxFreeSockets: 1,
          maxCachedSessions: 0,
          ca: tls.ca,
          cert: tls.cert,
          key: tls.key,
          minVersion: "TLSv1.3",
          rejectUnauthorized: true,
          checkServerIdentity(host, cert) {
            return (
              checkServerIdentity(host, cert) ??
              (cert.fingerprint256 === tls.serverFingerprint ? undefined : failure())
            );
          },
        });
        const response = await exchange(
          "/identity",
          undefined,
          IDENTITY_BYTES,
          Date.now() + 10000,
          operationSignal,
        );
        const identity = identityFrom(response.value);
        assertWorkerPolicy(identity, policy);
        if (response.socket.destroyed) throw failure();
        channel = { socket: response.socket, identity };
        state = "ready";
        idleTimer = setTimeout(close, CHANNEL_MS);
        idleTimer.unref();
        return { ...identity };
      } catch {
        close();
        throw failure();
      }
    },
    async dispatch(input: WorkerRequest): Promise<WorkerResponse> {
      if (state !== "ready" || !channel) throw failure();
      const verified = channel;
      state = "dispatch";
      clearTimeout(idleTimer);
      try {
        const request = parseWorkerRequest(structuredClone(input));
        const { intent, operation } = request;
        const now = Date.now();
        if (
          !["execute", "discover", "validateSchemas", "validateDestination", "cancel"].includes(
            operation.kind,
          ) ||
          intent.bootId !== verified.identity.bootId ||
          intent.zoneId !== verified.identity.zoneId ||
          intent.policyDigest !== verified.identity.policyDigest ||
          intent.operationDigest !== digest(operation) ||
          intent.destination !== ("endpoint" in operation ? operation.endpoint : "") ||
          intent.expiresAt <= now ||
          intent.expiresAt > now + 300000 ||
          intent.lease.expiresAt < intent.expiresAt ||
          intent.lease.expiresAt > now + 300000 ||
          verified.socket.destroyed
        )
          throw failure();
        const body = JSON.stringify(request);
        if (Buffer.byteLength(body) > REQUEST_BYTES) throw failure();
        const maximumMs =
          operation.kind === "execute" ? 300000 : operation.kind === "cancel" ? 5000 : 10000;
        const { value } = await exchange(
          "/rpc",
          body,
          RESPONSE_BYTES,
          Math.min(intent.expiresAt, now + maximumMs),
          operationSignal,
          verified.socket,
        );
        if (
          !object(value) ||
          value.ok !== true ||
          canonical(value.identity) !== canonical(verified.identity) ||
          canonical(value.intent) !== canonical(intent) ||
          value.intentId !== intent.id ||
          value.requestDigest !== intent.requestDigest ||
          value.operationDigest !== intent.operationDigest ||
          value.leaseId !== intent.lease.id ||
          !("result" in value)
        )
          throw failure();
        return {
          ok: true,
          identity: verified.identity,
          intent,
          intentId: intent.id,
          requestDigest: intent.requestDigest,
          operationDigest: intent.operationDigest,
          leaseId: intent.lease.id,
          result: value.result,
        };
      } catch {
        throw failure();
      } finally {
        close();
      }
    },
    close,
  };
}
