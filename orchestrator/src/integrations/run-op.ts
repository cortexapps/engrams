/**
 * Server-side, sessionless integration invocation (the "IntegrationOp" seam).
 *
 * Two modes, both resolved coordinator-side from one connector:
 *   - {@link runIntegrationOp}            — Mode A: the coordinator makes the
 *     authenticated call; the credential never leaves it. The caller supplies the
 *     request (method/path/body), so no connector `operation` template is needed.
 *   - {@link resolveIntegrationCredential} — Mode B: the coordinator returns the
 *     resolved credential so an off-the-shelf SDK can run in-process (see
 *     ./clients.ts). The token crosses to the orchestrator tier, never to a guest.
 *
 * The orchestrator owns the connector catalog; it compiles a connector's
 * `credential` block into a wire `CredentialSpec` and the coordinator (the only
 * tier that can unseal/mint) executes — mirroring `IntegrationService.testConnector`.
 */

import { loadRegistry, type Connector } from "../connectors/registry.ts";
import { makeConnectorStore } from "../db/connectors.ts";
import { getDb } from "../db/client.ts";
import { integrationOp as defaultIntegrationOp } from "../control-plane/client.ts";
import { CredentialSpecSchema } from "../gen/engram/app/v1/integration_op_pb.ts";
import type { ResolvedCredential } from "../gen/engram/app/v1/integration_op_pb.ts";
import type { MessageInitShape } from "@bufbuild/protobuf";

/** The slice of the coordinator IntegrationOpService this module calls. */
export type IntegrationOpClient = Pick<
  typeof defaultIntegrationOp,
  "runIntegrationOp" | "resolveIntegrationCredential"
>;

export interface RunOpDeps {
  connectors?: { list(): Promise<ReadonlyArray<{ provider: string; config: unknown }>> };
  integrationOp?: IntegrationOpClient;
}

/** A request to issue against an integration's host (the credential is added
 * coordinator-side from the connector). */
export interface IntegrationOpRequest {
  method: string;
  /** Path + optional query, e.g. "/api/chat.postMessage". */
  path: string;
  /** Optional body; strings are UTF-8 encoded. */
  body?: Uint8Array | string;
  /** Defaults to "application/json" coordinator-side when a body is present. */
  contentType?: string;
}

export interface IntegrationOpResult {
  status: number;
  body: Uint8Array;
  contentType: string;
  truncated: boolean;
}

/** Compile a connector's credential block into the wire CredentialSpec. The
 * coordinator is connector-agnostic — it resolves from exactly this. */
function credentialSpec(c: Connector): MessageInitShape<typeof CredentialSpecSchema> {
  if (c.credential.source === "mint") {
    // The coordinator resolves the mint engine by PROVIDER id (not the kind).
    return { source: "mint", mintProvider: c.provider, injects: [] };
  }
  return {
    source: "inject",
    mintProvider: "",
    injects: c.credential.injects.map((i) => ({
      header: i.header,
      template: i.template ?? "{}",
      secretRef: i.secretRef,
    })),
  };
}

async function resolveConnector(provider: string, deps?: RunOpDeps): Promise<Connector> {
  const source = deps?.connectors ?? makeConnectorStore(getDb());
  const registry = await loadRegistry(source);
  const c = registry.get(provider);
  if (!c) throw new Error(`unknown connector "${provider}"`);
  return c;
}

/**
 * Mode A — issue one authenticated request against the connector's host. The
 * coordinator resolves the credential and makes the call; the secret stays sealed.
 */
export async function runIntegrationOp(
  provider: string,
  req: IntegrationOpRequest,
  deps?: RunOpDeps,
): Promise<IntegrationOpResult> {
  const client = deps?.integrationOp ?? defaultIntegrationOp;
  const c = await resolveConnector(provider, deps);
  const host = c.hosts[0];
  if (!host) throw new Error(`connector "${provider}" has no host`);
  const body =
    typeof req.body === "string" ? new TextEncoder().encode(req.body) : (req.body ?? new Uint8Array());
  const resp = await client.runIntegrationOp({
    provider,
    host,
    method: req.method,
    path: req.path,
    body,
    contentType: req.contentType ?? "",
    credential: credentialSpec(c),
  });
  return {
    status: resp.status,
    body: resp.body,
    contentType: resp.contentType,
    truncated: resp.truncated,
  };
}

/**
 * Mode B — resolve a connector's credential to its raw material so an in-process
 * SDK can use it (see {@link getIntegrationClient}). Audit-logged coordinator-side.
 */
export async function resolveIntegrationCredential(
  provider: string,
  deps?: RunOpDeps,
): Promise<ResolvedCredential> {
  const client = deps?.integrationOp ?? defaultIntegrationOp;
  const c = await resolveConnector(provider, deps);
  const resp = await client.resolveIntegrationCredential({
    provider,
    credential: credentialSpec(c),
  });
  if (!resp.credential) throw new Error(`no credential resolved for "${provider}"`);
  return resp.credential;
}
