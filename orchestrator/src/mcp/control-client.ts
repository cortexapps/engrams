import { createClient, ConnectError, Code, type Client } from "@connectrpc/connect";
import { createGrpcTransport } from "@connectrpc/connect-node";
import { checkServerIdentity } from "node:tls";
import { CredentialControl } from "../gen/engram/mcp/v1/control_pb.ts";

export type CredentialControlClient = Client<typeof CredentialControl>;
export interface CredentialControlTls {
  url: string;
  ca: string;
  cert: string;
  key: string;
  serverFingerprint: string;
}
/** Dedicated transport: explicit trust roots, both certificate identities, no retries. */
export function makeCredentialControlClient(tls: CredentialControlTls): CredentialControlClient {
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
  return createClient(
    CredentialControl,
    createGrpcTransport({
      baseUrl: tls.url,
      readMaxBytes: 32768,
      writeMaxBytes: 32768,
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
    }),
  );
}
