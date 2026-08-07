/** Google Workload Identity Federation token broker (ADR 0109). */

import { createPrivateKey, createSign, type KeyObject } from "node:crypto";

import type { IntegrationOidcKeyStore } from "../db/integration-oidc-keys.ts";
import type { GoogleCloudConnectionConfig } from "../db/integration-connections.ts";
import { isDeniedGoogleHost } from "./google-credential-denylist.ts";

const STS_URL = "https://sts.googleapis.com/v1/token";
const IAM_CREDENTIALS_ORIGIN = "https://iamcredentials.googleapis.com";
export const GOOGLE_OAUTH_SCOPES = {
  api: "https://www.googleapis.com/auth/cloud-platform",
  cloud_sql_admin: "https://www.googleapis.com/auth/sqlservice.admin",
  cloud_sql_login: "https://www.googleapis.com/auth/sqlservice.login",
} as const;
const SUBJECT_TOKEN_LIFETIME_SECONDS = 300;
// The host proxy refreshes minted credentials five minutes before expiry.
// Keep the Google access token short-lived but longer than that refresh window,
// so ordinary requests do not call the internal broker on every connection.
const ACCESS_TOKEN_LIFETIME_SECONDS = 900;

export interface WifIdentity {
  sessionId: string;
  organizationId: string;
  connectionId: string;
  userId: string;
  profileSnapshotId: string;
}

export interface GoogleAccessToken {
  accessToken: string;
  expiresAt: Date;
}

export interface GoogleWifBrokerDeps {
  keys: IntegrationOidcKeyStore;
  issuer: string;
  now?: () => Date;
  randomId?: () => string;
  fetch?: typeof fetch;
}

function base64Url(value: string | Buffer): string {
  return Buffer.from(value).toString("base64url");
}

export function googleOidcAudience(provider: string): string {
  return provider.startsWith("//iam.googleapis.com/")
    ? provider
    : `//iam.googleapis.com/${provider.replace(/^\/+/, "")}`;
}

/**
 * A full workload-identity provider resource.
 *
 * Google's own rule for a pool id and a provider id is 4-32 characters of
 * lowercase letters, digits and hyphens, starting with a letter. The looser
 * `[a-z0-9-]+` this used to allow accepted ids Google rejects — a one-character
 * id, or one starting with a digit or a hyphen — so the connection stored
 * cleanly and only failed later, during the operator's `gcloud` run.
 */
const WIF_PROVIDER_RESOURCE =
  /^\/\/iam\.googleapis\.com\/projects\/[0-9]+\/locations\/global\/workloadIdentityPools\/[a-z][a-z0-9-]{3,31}\/providers\/[a-z][a-z0-9-]{3,31}$/;

export function assertGoogleCloudConfig(value: Record<string, unknown>): GoogleCloudConnectionConfig {
  const allowedKeys = new Set([
    "workloadIdentityProvider",
    "serviceAccountEmail",
    "endpoints",
    "cloudSqlPostgresInstance",
  ]);
  const unknownKeys = Object.keys(value).filter((key) => !allowedKeys.has(key));
  if (unknownKeys.length > 0) {
    throw new Error(`Google Cloud config contains unsupported fields: ${unknownKeys.join(", ")}`);
  }
  const workloadIdentityProvider = value.workloadIdentityProvider;
  const serviceAccountEmail = value.serviceAccountEmail;
  const endpoints = value.endpoints;
  const cloudSqlPostgresInstance = value.cloudSqlPostgresInstance;
  if (
    typeof workloadIdentityProvider !== "string" ||
    !WIF_PROVIDER_RESOURCE.test(workloadIdentityProvider)
  ) {
    throw new Error("workload identity provider must be a full Google provider resource");
  }
  // Google reserves the `gcp-` prefix on both ids. A resource string carrying
  // one can never be created, so accepting it here only defers the failure to
  // the operator's `gcloud` run, after the connection is already stored.
  const reserved = workloadIdentityProvider
    .split("/")
    .some((segment) => segment.startsWith("gcp-"));
  if (reserved) {
    throw new Error("Google reserves the `gcp-` prefix for pool and provider ids");
  }
  if (
    typeof serviceAccountEmail !== "string" ||
    !/^[a-z0-9][a-z0-9-]{2,62}@[a-z0-9-]{1,63}\.iam\.gserviceaccount\.com$/.test(serviceAccountEmail)
  ) {
    throw new Error("service account email is invalid");
  }
  if (!Array.isArray(endpoints) || !endpoints.every((entry) => typeof entry === "string")) {
    throw new Error("endpoints must be a list of exact hostnames");
  }
  const normalized = [...new Set(endpoints.map((entry) => entry.toLowerCase()))].sort();
  for (const endpoint of normalized) {
    if (
      endpoint.includes("*") || endpoint.includes(":") || endpoint.includes("/") ||
      !/^(?=.{1,253}$)(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,63}$/.test(endpoint)
    ) {
      throw new Error(`endpoint "${endpoint}" must be an exact hostname`);
    }
    if (isDeniedGoogleHost(endpoint)) {
      throw new Error(`credential exchange endpoint "${endpoint}" cannot be guest-accessible`);
    }
  }
  if (
    cloudSqlPostgresInstance !== undefined &&
    (typeof cloudSqlPostgresInstance !== "string" ||
      !/^[a-z][a-z0-9-]{4,28}[a-z0-9]:[a-z0-9-]{1,64}:[a-z][a-z0-9-]{0,96}$/.test(
        cloudSqlPostgresInstance,
      ))
  ) {
    throw new Error("Cloud SQL PostgreSQL instance must be project:region:instance");
  }
  return {
    workloadIdentityProvider,
    serviceAccountEmail,
    endpoints: normalized,
    ...(cloudSqlPostgresInstance ? { cloudSqlPostgresInstance } : {}),
  };
}

export function makeGoogleWifBroker(deps: GoogleWifBrokerDeps) {
  const now = deps.now ?? (() => new Date());
  const randomId = deps.randomId ?? (() => crypto.randomUUID());
  const fetchFn = deps.fetch ?? fetch;
  // Parse each signing key's PEM once per kid instead of on every mint. A
  // deployment publishes at most a handful of kids (active + retiring), so
  // reset the cache if it ever grows past that.
  const keyObjects = new Map<string, KeyObject>();
  function keyObjectFor(kid: string, privateKeyPem: string): KeyObject {
    let cached = keyObjects.get(kid);
    if (!cached) {
      if (keyObjects.size >= 8) keyObjects.clear();
      cached = createPrivateKey(privateKeyPem);
      keyObjects.set(kid, cached);
    }
    return cached;
  }

  async function mintSubjectToken(
    config: GoogleCloudConnectionConfig,
    identity: WifIdentity,
  ): Promise<string> {
    const issuedAt = Math.floor(now().getTime() / 1000);
    const key = await deps.keys.getOrCreateActive(new Date(issuedAt * 1000));
    const header = base64Url(JSON.stringify({ alg: "RS256", typ: "JWT", kid: key.kid }));
    const payload = base64Url(JSON.stringify({
      iss: deps.issuer,
      sub: identity.sessionId,
      aud: googleOidcAudience(config.workloadIdentityProvider),
      iat: issuedAt,
      nbf: issuedAt - 5,
      exp: issuedAt + SUBJECT_TOKEN_LIFETIME_SECONDS,
      jti: randomId(),
      engrams_organization: identity.organizationId,
      engrams_connection: identity.connectionId,
      engrams_user: identity.userId,
      engrams_profile_snapshot: identity.profileSnapshotId,
    }));
    const input = `${header}.${payload}`;
    const signer = createSign("RSA-SHA256");
    signer.update(input);
    signer.end();
    return `${input}.${signer.sign(keyObjectFor(key.kid, key.privateKeyPem), "base64url")}`;
  }

  async function exchange(
    config: GoogleCloudConnectionConfig,
    identity: WifIdentity,
    scopes: readonly string[] = [GOOGLE_OAUTH_SCOPES.api],
  ): Promise<GoogleAccessToken> {
    const subjectToken = await mintSubjectToken(config, identity);
    const stsResponse = await fetchFn(STS_URL, {
      method: "POST",
      headers: { "content-type": "application/x-www-form-urlencoded" },
      body: new URLSearchParams({
        audience: googleOidcAudience(config.workloadIdentityProvider),
        grant_type: "urn:ietf:params:oauth:grant-type:token-exchange",
        requested_token_type: "urn:ietf:params:oauth:token-type:access_token",
        scope: GOOGLE_OAUTH_SCOPES.api,
        subject_token_type: "urn:ietf:params:oauth:token-type:jwt",
        subject_token: subjectToken,
      }),
    });
    if (!stsResponse.ok) {
      throw new Error(`Google STS exchange failed with status ${stsResponse.status}`);
    }
    const sts = await stsResponse.json() as { access_token?: string };
    if (!sts.access_token) throw new Error("Google STS response did not contain an access token");

    const impersonationUrl =
      `${IAM_CREDENTIALS_ORIGIN}/v1/projects/-/serviceAccounts/` +
      `${encodeURIComponent(config.serviceAccountEmail)}:generateAccessToken`;
    const iamResponse = await fetchFn(impersonationUrl, {
      method: "POST",
      headers: {
        authorization: `Bearer ${sts.access_token}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({
        scope: [...scopes],
        lifetime: `${ACCESS_TOKEN_LIFETIME_SECONDS}s`,
      }),
    });
    if (!iamResponse.ok) {
      throw new Error(`Google service account impersonation failed with status ${iamResponse.status}`);
    }
    const iam = await iamResponse.json() as { accessToken?: string; expireTime?: string };
    if (!iam.accessToken || !iam.expireTime) {
      throw new Error("Google IAM Credentials response was incomplete");
    }
    return { accessToken: iam.accessToken, expiresAt: new Date(iam.expireTime) };
  }

  return { mintSubjectToken, exchange };
}

/**
 * The operator-facing setup document for one Google Cloud connection.
 *
 * It used to live in the Connect RPC handler, which made the RPC layer the
 * only place that knew how to describe a provider. It belongs with the rest of
 * the Google WIF knowledge, behind the provider seam.
 */
export function googleSetupDoc(
  row: { id: string; config: Record<string, unknown> },
  issuer: string,
  deploymentId: string,
): { audience: string; gcloudScript: string; terraform: string } {
  const google = assertGoogleCloudConfig(row.config);
  const match = google.workloadIdentityProvider.match(
    /^\/\/iam\.googleapis\.com\/projects\/([0-9]+)\/locations\/global\/workloadIdentityPools\/([a-z0-9-]+)\/providers\/([a-z0-9-]+)$/,
  );
  if (!match) throw new Error("stored Google provider resource is invalid");
  const [, projectNumber, poolId, providerId] = match;
  // Terraform resource names are addresses, not labels: two connections in the
  // same project used to emit `google_iam_workload_identity_pool.engrams`
  // twice, so applying the second setup silently redefined the first. Derive
  // the address from the provider id, which is already unique per connection.
  const tfName = `engrams_${providerId!.replace(/-/g, "_")}`;
  // `engrams_organization` carries the deployment id (the issuer URL already
  // rides in `issuer_uri`, so pinning the URL again added nothing).
  const condition =
    `assertion.engrams_organization == '${deploymentId}' && ` +
    `assertion.engrams_connection == '${row.id}'`;
  const mapping =
    "google.subject=assertion.sub," +
    "attribute.engrams_organization=assertion.engrams_organization," +
    "attribute.engrams_connection=assertion.engrams_connection";
  const principalSet =
    `principalSet://iam.googleapis.com/projects/${projectNumber}/locations/global/` +
    `workloadIdentityPools/${poolId}/attribute.engrams_connection/${row.id}`;
  const cloudSql = google.cloudSqlPostgresInstance?.split(":");
  const cloudSqlProject = cloudSql?.[0];
  const cloudSqlInstance = cloudSql?.[2];
  const cloudSqlCondition = cloudSqlProject && cloudSqlInstance
    ? `resource.name == 'projects/${cloudSqlProject}/instances/${cloudSqlInstance}' && resource.service == 'sqladmin.googleapis.com'`
    : undefined;
  const cloudSqlDatabaseUser = google.serviceAccountEmail.replace(/\.gserviceaccount\.com$/, "");
  const gcloudCloudSql = cloudSqlCondition
    ? [
        ``,
        `# Cloud SQL transport and automatic IAM database login, limited to one instance.`,
        `# First enable the cloudsql.iam_authentication database flag without replacing`,
        `# any existing flags on the instance. Restart it if Google requires one.`,
        `for role in roles/cloudsql.client roles/cloudsql.instanceUser; do`,
        `  gcloud projects add-iam-policy-binding ${cloudSqlProject} --member=serviceAccount:${google.serviceAccountEmail} --role="$role" --condition="expression=${cloudSqlCondition},title=engrams-${row.id}"`,
        `done`,
        ``,
        `gcloud sql users create ${cloudSqlDatabaseUser} --project=${cloudSqlProject} --instance=${cloudSqlInstance} --type=cloud_iam_service_account`,
        ``,
        `# Grant this database user only CONNECT, schema USAGE, and table SELECT.`,
        `# Set default_transaction_read_only=on and a statement_timeout as defense`,
        `# in depth. PostgreSQL grants enforce read-only access.`,
      ]
    : [];
  const terraformCloudSql = cloudSqlCondition
    ? [
        ``,
        `resource "google_project_iam_member" "${tfName}_cloud_sql_client" {`,
        `  project = "${cloudSqlProject}"`,
        `  role    = "roles/cloudsql.client"`,
        `  member  = "serviceAccount:${google.serviceAccountEmail}"`,
        `  condition {`,
        `    title      = "engrams-${row.id}"`,
        `    expression = "${cloudSqlCondition}"`,
        `  }`,
        `}`,
        ``,
        `resource "google_project_iam_member" "${tfName}_cloud_sql_instance_user" {`,
        `  project = "${cloudSqlProject}"`,
        `  role    = "roles/cloudsql.instanceUser"`,
        `  member  = "serviceAccount:${google.serviceAccountEmail}"`,
        `  condition {`,
        `    title      = "engrams-${row.id}"`,
        `    expression = "${cloudSqlCondition}"`,
        `  }`,
        `}`,
        ``,
        `resource "google_sql_user" "${tfName}_database_user" {`,
        `  project  = "${cloudSqlProject}"`,
        `  instance = "${cloudSqlInstance}"`,
        `  name     = "${cloudSqlDatabaseUser}"`,
        `  type     = "CLOUD_IAM_SERVICE_ACCOUNT"`,
        `}`,
      ]
    : [];
  return {
    audience: google.workloadIdentityProvider,
    // An operator pastes this into a shell. Without a shebang the lines run
    // under whatever shell they happen to use, and without `set -euo pipefail`
    // a failed pool creation is invisible: the next command runs anyway and the
    // script "succeeds" with a half-built pool. Pool and provider creation are
    // describe-then-create so re-running the setup — the normal thing to do
    // after editing endpoints — is not an ALREADY_EXISTS error.
    gcloudScript: [
      `#!/usr/bin/env bash`,
      `set -euo pipefail`,
      ``,
      `gcloud iam workload-identity-pools describe ${poolId} --location=global --project=${projectNumber} >/dev/null 2>&1 ||`,
      `  gcloud iam workload-identity-pools create ${poolId} --location=global --project=${projectNumber}`,
      ``,
      `gcloud iam workload-identity-pools providers describe ${providerId} --location=global --workload-identity-pool=${poolId} --project=${projectNumber} >/dev/null 2>&1 ||`,
      `  gcloud iam workload-identity-pools providers create-oidc ${providerId} --location=global --workload-identity-pool=${poolId} --project=${projectNumber} --issuer-uri=${issuer} --allowed-audiences=${google.workloadIdentityProvider} --attribute-mapping=${mapping} --attribute-condition=\"${condition}\"`,
      ``,
      `gcloud iam service-accounts add-iam-policy-binding ${google.serviceAccountEmail} --project=${projectNumber} --role=roles/iam.workloadIdentityUser --member=${principalSet}`,
      ...gcloudCloudSql,
    ].join("\n"),
    terraform: [
      `resource "google_iam_workload_identity_pool" "${tfName}" {`,
      `  project                   = "${projectNumber}"`,
      `  workload_identity_pool_id = "${poolId}"`,
      `}`,
      ``,
      `resource "google_iam_workload_identity_pool_provider" "${tfName}" {`,
      `  project                            = "${projectNumber}"`,
      `  workload_identity_pool_id          = google_iam_workload_identity_pool.${tfName}.workload_identity_pool_id`,
      `  workload_identity_pool_provider_id = "${providerId}"`,
      `  attribute_mapping = {`,
      `    "google.subject"                 = "assertion.sub"`,
      `    "attribute.engrams_organization" = "assertion.engrams_organization"`,
      `    "attribute.engrams_connection"   = "assertion.engrams_connection"`,
      `  }`,
      `  attribute_condition = "${condition}"`,
      `  oidc {`,
      `    issuer_uri        = "${issuer}"`,
      `    allowed_audiences = ["${google.workloadIdentityProvider}"]`,
      `  }`,
      `}`,
      ``,
      `resource "google_service_account_iam_member" "${tfName}" {`,
      `  service_account_id = "projects/${projectNumber}/serviceAccounts/${google.serviceAccountEmail}"`,
      `  role               = "roles/iam.workloadIdentityUser"`,
      `  member             = "${principalSet}"`,
      `}`,
      ...terraformCloudSql,
    ].join("\n"),
  };
}
