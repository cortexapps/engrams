import { createClient, ConnectError, Code, type Client } from "@connectrpc/connect";
import { createGrpcTransport } from "@connectrpc/connect-node";
import { checkServerIdentity } from "node:tls";
import {
  CredentialControlService,
  type CredentialStatus,
} from "../gen/engram/mcp/v1/control_pb.ts";

import { SessionPolicyService } from "../gen/engram/mcp/v1/session_policy_pb.ts";

type CredentialWireClient = Client<typeof CredentialControlService>;
type StatusMethod = "putSecret" | "completeAuthorization" | "getStatus" | "revoke";
export type CredentialControlClient = Omit<CredentialWireClient, StatusMethod> & {
  [K in StatusMethod]: (...args: Parameters<CredentialWireClient[K]>) => Promise<CredentialStatus>;
};
/** Keep one status DTO at the lifecycle boundary; missing wire status is never ready. */
export function unwrapCredentialControlClient(wire: CredentialWireClient): CredentialControlClient {
  function status(response: { status?: CredentialStatus }): CredentialStatus {
    if (!response.status)
      throw new ConnectError("credential control status unavailable", Code.DataLoss);
    return response.status;
  }
  return {
    ...wire,
    putSecret: async (...args) => status(await wire.putSecret(...args)),
    completeAuthorization: async (...args) => status(await wire.completeAuthorization(...args)),
    getStatus: async (...args) => status(await wire.getStatus(...args)),
    revoke: async (...args) => status(await wire.revoke(...args)),
  };
}
export type SessionPolicyControlClient = Client<typeof SessionPolicyService>;
export interface CredentialControlTls {
  url: string;
  ca: string;
  cert: string;
  key: string;
  serverFingerprint: string;
}
/** Dedicated transport: explicit trust roots, both certificate identities, no retries. */
export function makeCredentialControlClient(tls: CredentialControlTls): CredentialControlClient {
  return unwrapCredentialControlClient(
    createClient(CredentialControlService, makeControlTransport(tls, 32768)),
  );
}
export function makeSessionPolicyControlClient(
  tls: CredentialControlTls,
): Client<typeof SessionPolicyService> {
  // Match the coordinator policy service: bounded JSON commands can occupy 8 MiB.
  return createClient(SessionPolicyService, makeControlTransport(tls, 9 * 1024 * 1024));
}
function makeControlTransport(tls: CredentialControlTls, maximumBytes: number) {
  const url = new URL(tls.url);
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
    !/^(?:[0-9A-F]{2}:){31}[0-9A-F]{2}$/.test(tls.serverFingerprint)
  )
    throw new Error("MCP credential control requires pinned mTLS");
  return createGrpcTransport({
    baseUrl: tls.url,
    readMaxBytes: maximumBytes,
    writeMaxBytes: maximumBytes,
    defaultTimeoutMs: 30000,
    idleConnectionTimeoutMs: 1000,
    nodeOptions: {
      ca: tls.ca,
      cert: tls.cert,
      key: tls.key,
      minVersion: "TLSv1.3",
      rejectUnauthorized: true,
      checkServerIdentity(host, cert) {
        return (
          checkServerIdentity(host, cert) ??
          (cert.fingerprint256 === tls.serverFingerprint
            ? undefined
            : new Error("MCP control certificate mismatch"))
        );
      },
    },
    interceptors: [
      (next) => async (request) => {
        try {
          return await next(request);
        } catch (error) {
          throw new ConnectError(
            "MCP credential control unavailable",
            error instanceof ConnectError ? error.code : Code.Unavailable,
          );
        }
      },
    ],
  });
}
