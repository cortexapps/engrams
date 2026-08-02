/** Google Workload Identity Federation token broker (ADR 0109). */

import { createSign } from "node:crypto";

import type { IntegrationOidcKeyStore } from "../db/integration-oidc-keys.ts";
import type { GoogleCloudConnectionConfig } from "../db/integration-connections.ts";
import { isDeniedGoogleHost } from "./google-credential-denylist.ts";

const STS_URL = "https://sts.googleapis.com/v1/token";
const IAM_CREDENTIALS_ORIGIN = "https://iamcredentials.googleapis.com";
const CLOUD_PLATFORM_SCOPE = "https://www.googleapis.com/auth/cloud-platform";
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

export function assertGoogleCloudConfig(value: Record<string, unknown>): GoogleCloudConnectionConfig {
  const allowedKeys = new Set(["workloadIdentityProvider", "serviceAccountEmail", "endpoints"]);
  const unknownKeys = Object.keys(value).filter((key) => !allowedKeys.has(key));
  if (unknownKeys.length > 0) {
    throw new Error(`Google Cloud config contains unsupported fields: ${unknownKeys.join(", ")}`);
  }
  const workloadIdentityProvider = value.workloadIdentityProvider;
  const serviceAccountEmail = value.serviceAccountEmail;
  const endpoints = value.endpoints;
  if (
    typeof workloadIdentityProvider !== "string" ||
    !/^\/\/iam\.googleapis\.com\/projects\/[0-9]+\/locations\/global\/workloadIdentityPools\/[a-z0-9-]+\/providers\/[a-z0-9-]+$/.test(workloadIdentityProvider)
  ) {
    throw new Error("workload identity provider must be a full Google provider resource");
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
  return { workloadIdentityProvider, serviceAccountEmail, endpoints: normalized };
}

export function makeGoogleWifBroker(deps: GoogleWifBrokerDeps) {
  const now = deps.now ?? (() => new Date());
  const randomId = deps.randomId ?? (() => crypto.randomUUID());
  const fetchFn = deps.fetch ?? fetch;

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
    return `${input}.${signer.sign(key.privateKeyPem, "base64url")}`;
  }

  async function exchange(
    config: GoogleCloudConnectionConfig,
    identity: WifIdentity,
  ): Promise<GoogleAccessToken> {
    const subjectToken = await mintSubjectToken(config, identity);
    const stsResponse = await fetchFn(STS_URL, {
      method: "POST",
      headers: { "content-type": "application/x-www-form-urlencoded" },
      body: new URLSearchParams({
        audience: googleOidcAudience(config.workloadIdentityProvider),
        grant_type: "urn:ietf:params:oauth:grant-type:token-exchange",
        requested_token_type: "urn:ietf:params:oauth:token-type:access_token",
        scope: CLOUD_PLATFORM_SCOPE,
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
        scope: [CLOUD_PLATFORM_SCOPE],
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
