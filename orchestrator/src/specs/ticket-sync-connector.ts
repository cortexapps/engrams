/**
 * Is Linear connected, and where do this org's tickets go by default (R42, R45)?
 *
 * The answer to the first question is not ours to invent: an OAuth-facet
 * connector's status comes solely from the coordinator's sealed credential
 * store, exactly as the connector catalog reports it. A missing or rejected
 * credential is therefore reported as "not connected" with the reason, and the
 * ticket tree stays fully editable around it (R42) — the sync call is the only
 * thing that fails, and it fails by pointing at the connect page.
 *
 * The default team, project and labels ride on the provider's default
 * integration connection. A spec may override them, and that override lands in
 * `spec_ticket_sync_config`, never here (R45).
 */

import { oauthCredential } from "../control-plane/client.ts";
import { getDb } from "../db/client.ts";
import { makeConnectorStore } from "../db/connectors.ts";
import { makeIntegrationConnectionStore } from "../db/integration-connections.ts";
import {
  connectorStatus,
  loadRegistry,
  type OauthCredentialStatus,
} from "../connectors/registry.ts";
import { OauthSubjectKind } from "../gen/engram/app/v1/oauth_pb.ts";
import {
  EMPTY_SYNC_TARGET,
  type SpecTicketSyncConnector,
  type SpecTicketSyncConnectorState,
  type SpecTicketSyncTarget,
} from "./ticket-sync.ts";

export const LINEAR_PROVIDER = "linear";

/** The org connector's default target, as stored on its default connection. */
export function readConnectionTarget(config: Record<string, unknown>): SpecTicketSyncTarget {
  return {
    teamId: text(config["teamId"]),
    teamName: text(config["teamName"]),
    projectId: text(config["projectId"]),
    projectName: text(config["projectName"]),
    labelIds: textList(config["labelIds"]),
    labelNames: textList(config["labelNames"]),
  };
}

export function makeSpecTicketSyncConnector(deps?: {
  listOauthStatus?: () => Promise<Map<string, OauthCredentialStatus>>;
  readDefaults?: () => Promise<SpecTicketSyncTarget>;
}): SpecTicketSyncConnector {
  const listOauthStatus = deps?.listOauthStatus ?? productionOauthStatus;
  const readDefaults = deps?.readDefaults ?? productionDefaults;

  return {
    async read(): Promise<SpecTicketSyncConnectorState> {
      const registry = await loadRegistry(makeConnectorStore(getDb()));
      const connector = registry.get(LINEAR_PROVIDER);
      if (!connector) {
        return {
          connected: false,
          reason: "Linear is not in this organization's connector catalog.",
          defaults: EMPTY_SYNC_TARGET,
        };
      }
      const statuses = await listOauthStatus();
      const status = connectorStatus(
        connector,
        new Set<string>(),
        [],
        statuses.get(LINEAR_PROVIDER),
      );
      if (status !== "connected") {
        return {
          connected: false,
          reason:
            status === "needs_reconnect"
              ? "Linear rejected the workspace credential. Reconnect Linear in Settings → Integrations."
              : "Linear is not connected yet. Connect it in Settings → Integrations.",
          defaults: EMPTY_SYNC_TARGET,
        };
      }
      return { connected: true, reason: null, defaults: await readDefaults() };
    },
  };
}

async function productionOauthStatus(): Promise<Map<string, OauthCredentialStatus>> {
  const byProvider = new Map<string, OauthCredentialStatus>();
  const response = await oauthCredential
    // The kind-wide listing: every connector credential in one call.
    .listCredentials({ subject: { kind: OauthSubjectKind.CONNECTOR, id: "" } })
    .catch(() => null);
  for (const credential of response?.credentials ?? []) {
    byProvider.set(credential.provider, credential.status as OauthCredentialStatus);
  }
  return byProvider;
}

async function productionDefaults(): Promise<SpecTicketSyncTarget> {
  const connection = await makeIntegrationConnectionStore(getDb()).getDefault(LINEAR_PROVIDER);
  return connection ? readConnectionTarget(connection.config) : EMPTY_SYNC_TARGET;
}

function text(value: unknown): string | null {
  return typeof value === "string" && value !== "" ? value : null;
}

function textList(value: unknown): string[] {
  return Array.isArray(value) ? value.filter((entry): entry is string => typeof entry === "string") : [];
}
