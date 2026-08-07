/**
 * Google Cloud as a `ConnectionProvider` (ADR 0109).
 *
 * Every Google-specific decision the orchestrator makes now enters through
 * this file: config validation, the operation catalog, grant validation,
 * policy compilation, minting, and the operator setup document. Callers hold a
 * `ConnectionProvider` and never a Google type.
 *
 * The bodies still live in `google-policy.ts` and `google-wif.ts` — this is a
 * seam, not a rewrite. What changed is that nothing outside it names `"gcp"`.
 */

import { ConnectError, Code } from "@connectrpc/connect";

import type { IntegrationPolicyJson } from "../../connectors/registry.ts";
import type { ResolvedIntegrationGrant } from "../grants.ts";
import {
  CURATED_GOOGLE_OPERATIONS,
  FORBIDDEN_GOOGLE_OPERATIONS,
  GOOGLE_PASSTHROUGH_CATALOG,
  GOOGLE_PASSTHROUGH_OPERATIONS,
  CLOUD_SQL_POSTGRES_CONNECT,
  appendGooglePolicy,
  validateGoogleGrants,
} from "../google-policy.ts";
import { assertGoogleCloudConfig, GOOGLE_OAUTH_SCOPES, googleSetupDoc } from "../google-wif.ts";
import type {
  ConnectionProvider,
  CredentialPurpose,
  MintIdentity,
  MintedCredential,
  ProviderConnection,
  ProviderSetupContext,
  ProviderSetupDoc,
} from "./provider.ts";

/** How the Google broker is reached. Injected so tests need no network. */
export interface GoogleProviderDeps {
  exchange(
    config: ReturnType<typeof assertGoogleCloudConfig>,
    identity: MintIdentity,
    scopes?: readonly string[],
  ): Promise<{ accessToken: string; expiresAt: Date }>;
}

export function makeGoogleProvider(deps: GoogleProviderDeps): ConnectionProvider {
  return {
    key: "gcp",
    displayName: "Google Cloud",
    blurb: "Call Google Cloud APIs with a short-lived, policy-bound credential.",
    category: "cloud",
    cli: {
      displayName: "Google Cloud",
      bins: ["gcloud", "engram-cloud-sql-proxy"],
      doc: "Use brokered metadata ADC. For Cloud SQL, start engram-cloud-sql-proxy. Do not log in or create credentials.",
    },
    operations: {
      curated: [...Object.keys(CURATED_GOOGLE_OPERATIONS), CLOUD_SQL_POSTGRES_CONNECT],
      passthrough: [...GOOGLE_PASSTHROUGH_OPERATIONS],
      forbidden: FORBIDDEN_GOOGLE_OPERATIONS,
      describe: [
        {
          action: CLOUD_SQL_POSTGRES_CONNECT,
          label: "Connect to Cloud SQL PostgreSQL (database role controls access)",
          access: "write",
          host: null,
          endpointRule: null,
        },
        ...Object.entries(CURATED_GOOGLE_OPERATIONS).map(([action, policy]) => ({
          action,
          label: policy.label,
          access: policy.access,
          host: policy.host,
          endpointRule: null,
        })),
        ...GOOGLE_PASSTHROUGH_OPERATIONS.map((action) => ({
          action,
          label: GOOGLE_PASSTHROUGH_CATALOG[action].label,
          access: GOOGLE_PASSTHROUGH_CATALOG[action].access,
          host: null,
          endpointRule: GOOGLE_PASSTHROUGH_CATALOG[action].endpoint,
        })),
      ],
    },
    credentialPurposes: ["cloud_sql_admin", "cloud_sql_login"],
    // Google delivers its credential through a metadata service, so a session
    // holding one of these connections needs the host-side endpoint.
    metadataFlavor: "gce",
    guestEnv: {
      // The Cloud SDK reads GCE_METADATA_ROOT while google-auth reads
      // GCE_METADATA_HOST. Point every client at the session-local emulator.
      GCE_METADATA_HOST: "169.254.169.254",
      GCE_METADATA_IP: "169.254.169.254",
      GCE_METADATA_ROOT: "169.254.169.254",
      CLOUDSDK_CORE_CHECK_GCE_METADATA: "true",
    },
    guestBundles: ["integrations-cli"],

    validateConfig(value: Record<string, unknown>): Record<string, unknown> {
      return assertGoogleCloudConfig(value) as unknown as Record<string, unknown>;
    },

    validateGrants(resolved: readonly ResolvedIntegrationGrant[]): void {
      validateGoogleGrants(resolved);
    },

    compilePolicy(
      policy: IntegrationPolicyJson,
      resolved: readonly ResolvedIntegrationGrant[],
    ): void {
      appendGooglePolicy(policy, resolved);
    },

    async mint(
      connection: ProviderConnection,
      identity: MintIdentity,
      purpose: CredentialPurpose = "api",
    ): Promise<MintedCredential> {
      const config = assertGoogleCloudConfig(connection.config);
      if (purpose !== "api" && !config.cloudSqlPostgresInstance) {
        throw new Error("Cloud SQL credential requested without a configured instance");
      }
      const token = await deps.exchange(config, identity, [GOOGLE_OAUTH_SCOPES[purpose]]);
      return { kind: "bearer", token: token.accessToken, expiresAt: token.expiresAt };
    },

    setupDoc(
      connection: ProviderConnection,
      context: ProviderSetupContext,
    ): ProviderSetupDoc {
      return googleSetupDoc(connection, context.issuer, context.deploymentId);
    },

    assertDeploymentReady(context: ProviderSetupContext): void {
      // Google fetches the issuer's discovery and JWKS documents itself, so a
      // deployment behind a private or credentialed URL can never complete the
      // exchange. Refuse at enable time rather than at first mint.
      let url: URL;
      try {
        url = new URL(context.issuer);
      } catch {
        throw new ConnectError(
          "Google Cloud requires a public HTTPS issuer",
          Code.FailedPrecondition,
        );
      }
      if (url.protocol !== "https:" || url.username || url.password || !url.hostname) {
        throw new ConnectError(
          "Google Cloud WIF requires a public issuer without URL credentials",
          Code.FailedPrecondition,
        );
      }
    },

    auditIdentity(connection: ProviderConnection): string | undefined {
      return connection.config.serviceAccountEmail as string | undefined;
    },
  };
}
