/**
 * The named-connection provider seam (ADR 0109).
 *
 * A *named connection* is an integration an administrator configures once as a
 * row — a Google Cloud project, and later an AWS account or an Azure
 * subscription — as opposed to the org-wide connectors keyed by a single
 * secret. Everything that differs between such providers lives behind this
 * interface, and every caller reaches it through the registry.
 *
 * ADR 0109 said "no runtime branches on the provider". The first implementation
 * did not hold to that: `provider === "gcp"` appeared at nine call sites across
 * grant validation, policy compilation, the create RPC, session create and the
 * credential broker, plus a `startsWith("gcp:")` in profile save — the exact
 * shape the ADR forbids. Each one was a place a second provider would have to
 * be threaded through by hand, and a place the two could disagree.
 *
 * The seam is deliberately narrow. It carries only what genuinely differs:
 * how a config is validated, which operations exist, how grants compile to
 * policy, how a credential is minted, and what the operator has to run. Session
 * lifetime, authorization, caching, audit logging and the egress policy shape
 * are provider-neutral and stay with their callers.
 */

import type { ConnectError } from "@connectrpc/connect";

/**
 * The minimum a provider needs to know about a connection: which one it is,
 * and how it is configured.
 *
 * Deliberately narrower than the live `integration_connection` row. The
 * credential broker mints from the session's IMMUTABLE snapshot, not from the
 * current row — a connection edited after a session booted must not change
 * what that session can do — and the snapshot carries only these fields.
 */
export interface ProviderConnection {
  id: string;
  alias: string;
  provider: string;
  displayName: string;
  config: Record<string, unknown>;
}
import type { IntegrationPolicyJson } from "../../connectors/registry.ts";
import type { ResolvedIntegrationGrant } from "../grants.ts";

/** A short-lived credential the proxy injects on the guest's behalf. */
export interface MintedCredential {
  /** Injection scheme. Only `bearer` exists today. */
  kind: "bearer";
  token: string;
  expiresAt: Date;
}

/** Who the credential is minted for. Provider-neutral. */
export interface MintIdentity {
  sessionId: string;
  organizationId: string;
  connectionId: string;
  userId: string;
  /** Content hash of the compiled profile snapshot this session booted with. */
  profileSnapshotId: string;
}

/** Fixed credential uses. Providers map these to checked-in OAuth scopes. */
/** Provider-owned fixed credential use. It is never an OAuth scope. */
export type CredentialPurpose = string;

/** The operator-facing setup document for one connection. */
export interface ProviderSetupDoc {
  /** The token audience the provider's trust policy must accept. */
  audience: string;
  /** A script the operator pastes into a shell. */
  gcloudScript: string;
  /** The same configuration as infrastructure-as-code. */
  terraform: string;
}

/** What `setupDoc` needs from the deployment, rather than from the row. */
export interface ProviderSetupContext {
  /** Public OIDC issuer this deployment publishes. */
  issuer: string;
  /** Deployment identity carried in the `engrams_organization` claim. */
  deploymentId: string;
}

/**
 * The operations a provider offers.
 *
 * `forbidden` is the important one: operations that can produce a credential
 * outliving the session are refused at GRANT time, before anything is stored.
 * The egress proxy refuses them again at request time from its own checked-in
 * table — two independent enforcement points, deliberately.
 */
export interface ProviderOperationCatalog {
  /** Curated operations, each compiling to a narrow host + path + method set. */
  readonly curated: readonly string[];
  /** Broad operations scoped only by the connection's configured endpoints. */
  readonly passthrough: readonly string[];
  /** Operations that can mint a credential; never grantable. */
  readonly forbidden: ReadonlySet<string>;
  /**
   * What an operator sees, per operation. Served through the integration
   * catalog so the web renders from THIS table rather than keeping its own —
   * two hand-maintained copies of an operation list drift, and the copy the
   * UI reads is the one that decides what an administrator can grant.
   */
  readonly describe: readonly ProviderOperationDescription[];
}

export interface ProviderOperationDescription {
  action: string;
  label: string;
  access: "read" | "write";
  /**
   * The exact host this operation calls, or `null` when its reach comes from
   * the connection's own endpoint list.
   */
  host: string | null;
  /**
   * For a host-less operation, the kind of endpoint that makes it usable.
   * `null` for a curated operation, which needs its exact `host`.
   */
  endpointRule: "google-api" | "non-google-api" | null;
}

/** The guest-side CLI a connection to this provider makes usable. */
export interface ProviderCliSurface {
  readonly displayName: string;
  readonly bins: readonly string[];
  readonly doc: string;
}

export interface ConnectionProvider {
  /** Stable key, matching `integration_connection.provider`. */
  readonly key: string;
  /** Operator-facing name. */
  readonly displayName: string;
  /** Short description for the connector catalog. */
  readonly blurb: string;
  /** Catalog grouping. */
  readonly category: string;
  readonly cli: ProviderCliSurface;
  readonly operations: ProviderOperationCatalog;
  /**
   * Non-default host-only credential uses and the operations that authorize
   * each one. The broker rejects every purpose absent from this map.
   */
  readonly credentialPurposes?: Readonly<Record<string, readonly string[]>>;

  /**
   * Validate and normalize a stored config. Throws on anything invalid,
   * including any field that could hold a secret — a named connection carries
   * configuration, never a credential.
   */
  validateConfig(value: Record<string, unknown>): Record<string, unknown>;

  /**
   * Validate grant SHAPE: the operation exists and its resource constraints
   * parse. PURE — it must not read connection state, because profile save
   * calls it and a connection an administrator later disables must never block
   * unrelated edits of every profile that grants it.
   */
  validateGrants(resolved: readonly ResolvedIntegrationGrant[]): void;

  /**
   * Compile grants into policy inject entries at session-create. This is where
   * connection STATE is enforced (enabled, endpoint membership, config
   * validity), because that state is only load-bearing when a session boots.
   */
  compilePolicy(
    policy: IntegrationPolicyJson,
    resolved: readonly ResolvedIntegrationGrant[],
  ): void;

  /** Exchange the deployment's identity for a short-lived credential. */
  mint(
    connection: ProviderConnection,
    identity: MintIdentity,
    purpose?: CredentialPurpose,
  ): Promise<MintedCredential>;

  /** What the operator has to configure on their side. */
  setupDoc(
    connection: ProviderConnection,
    context: ProviderSetupContext,
  ): ProviderSetupDoc;

  /**
   * Refuse to enable a connection when this deployment cannot support it —
   * a non-public issuer the provider could never fetch, say. Throws a
   * `ConnectError`; a provider with no such requirement omits it.
   */
  assertDeploymentReady?(context: ProviderSetupContext): void | never;

  /** Compatibility services this provider mounts on the guest gateway. */
  readonly guestServices?: readonly import("../../connectors/registry.ts").GuestService[];

  /**
   * Environment a guest needs in order to FIND this provider's credential.
   * Merged into the harness environment when a session holds one of these
   * connections. Never a credential — only where to look for one.
   */
  readonly guestEnv?: Readonly<Record<string, string>>;

  /** Session bundles a connection to this provider requires in the guest. */
  readonly guestBundles?: readonly string[];

  /** A field on the row that names the identity a credential acts as. */
  auditIdentity(connection: ProviderConnection): string | undefined;
}

/** Narrow a thrown value for callers that re-map provider errors. */
export type ProviderError = ConnectError | Error;
