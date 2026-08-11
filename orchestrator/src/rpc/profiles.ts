/**
 * Native ProfileService implementation (ADR 0053).
 *
 * Orchestrator-native (like TaskService): registered on the ConnectRouter,
 * NEVER proxied. Self-gates with the same CASL machinery as everything else.
 *
 * Field-level filtering (ADR §6): non-admin callers get env_vars stripped;
 * include_archived is admin-only. Mutations are admin-only.
 *
 * Image integrity (ADR §3): create/update validate image_id against the
 * coordinator's enabled-image catalog and reject an absent/disabled id.
 *
 * Injectable deps (getSession, store, images) for tests.
 */

import { ConnectError, Code } from "@connectrpc/connect";
import type { ConnectRouter } from "@connectrpc/connect";

import { ProfileService } from "../gen/engram/app/v1/profile_pb.ts";
import type { Profile } from "../gen/engram/app/v1/profile_pb.ts";

import { abilityFor } from "../authz/ability.ts";
import { getSessionFromHeaders } from "../auth/session.ts";
import { requireUser } from "./require.ts";
import { getDb } from "../db/client.ts";
import { makeProfileStore, type ProfileRow, type ProfileStore } from "../db/profiles.ts";
import {
  DEFAULT_PROFILE_NETWORK,
  type ProfileNetwork,
  type ProfileSecret,
  type ProfileIntegrationGrant,
} from "../db/schema.ts";
import {
  images as defaultImages,
  harnessCatalog as defaultHarnessCatalog,
} from "../control-plane/client.ts";
import {
  defaultCatalog,
  selectableSkillNames,
  type MountCatalogClient,
} from "../skills/catalog.ts";
import {
  parseCapability,
  grantsCapability,
  loadRegistry,
  type Connector,
  type CustomConnectorSource,
} from "../connectors/registry.ts";
import { makeConnectorStore } from "../db/connectors.ts";
import { toolCapabilities as registeredToolCapabilities } from "../tools/registry.ts";
import {
  makeIntegrationConnectionStore,
  type IntegrationConnectionStore,
} from "../db/integration-connections.ts";
import {
  grantsToCapabilities,
  resolveIntegrationGrants,
  type ResolvedIntegrationGrant,
} from "../integrations/grants.ts";
import {
  isProviderCapability,
  validateProviderGrants,
} from "../integrations/providers/index.ts";
import { normalizeRepos, type DiscoveredRepo } from "./profile-repos.ts";
import {
  discoverProfileRepos as runRepoDiscovery,
  DiscoverReposError,
  type DiscoverReposDeps,
} from "./profile-discover.ts";
import { sessions as defaultSessions } from "../control-plane/client.ts";

/** Subset of ImageService client used here (catalog validation). */
export interface ImagesClient {
  listEnabledImages(req: Record<string, never>): Promise<{
    images: Array<{ id: string; imageUri: string }>;
  }>;
}

/** Subset of HarnessCatalogService used here (ADR 0062/0063 catalog validation). */
export interface HarnessCatalogClient {
  listHarnesses(req: Record<string, never>): Promise<{
    harnesses: Array<{
      name: string;
      descriptor?: {
        models?: Array<{ id: string }>;
        effort?: Array<{ id: string }>;
      };
    }>;
  }>;
}

export type GetSession = (
  headers: Headers,
) => Promise<{ user: { id: string; role?: string | null; email?: string | null } } | null>;

export interface ProfileDeps {
  getSession?: GetSession;
  store?: ProfileStore;
  images?: ImagesClient;
  harnessCatalog?: HarnessCatalogClient;
  mountCatalog?: MountCatalogClient;
  connectors?: CustomConnectorSource;
  toolCapabilities?: Set<string>;
  connections?: IntegrationConnectionStore;
  /** Repo autodiscovery flow (boot → scan → tear down); injectable for tests. */
  discoverRepos?: (profileId: string) => Promise<DiscoveredRepo[]>;
}


/** Map a ProfileRow to the proto Profile. env_vars included only when admin. */
function toProto(row: ProfileRow, isAdmin: boolean): Profile {
  return {
    id: row.id,
    name: row.name,
    description: row.description,
    icon: row.icon,
    imageId: row.imageId,
    includeUserTokens: row.includeUserTokens,
    envVars: isAdmin ? row.envVars : {},
    // ADR 0055: skills are not sensitive (they describe granted tooling), so
    // they are surfaced to members too — unlike env_vars.
    skills: row.skills,
    integrationGrants: row.integrationGrants.map((grant) => ({
      connectionId: grant.connectionId,
      operation: grant.operation,
      resourceConstraints: grant.resourceConstraints,
      credentialScope: grant.credentialScope ?? "",
    })),
    // ADR 0057: network + secrets describe access/config (the secret VALUES
    // live in the org store, never here), so they're member-visible like skills.
    network: row.network,
    secrets: row.secrets,
    // Git checkouts in the image — member-visible config, like skills.
    repos: row.repos.map((r) => ({
      path: r.path,
      remoteUrl: r.remoteUrl,
      remote: r.remote ?? undefined,
    })),
    // ADR 0062/0063: default harness/model/effort — member-visible (they
    // describe a selection, not a secret).
    harness: row.harness ?? undefined,
    model: row.model ?? undefined,
    effort: row.effort ?? undefined,
    designation: row.designation ?? undefined,
    // ADR 0064: ports auto-exposed for this profile's sessions (member-visible —
    // describes config, not a secret, like skills).
    portExposures: row.portExposures,
    archived: row.deletedAt != null,
    createdAt: row.createdAt.toISOString(),
    updatedAt: row.updatedAt.toISOString(),
  } as Profile;
}

const ENV_NAME_RE = /^[A-Za-z_][A-Za-z0-9_]*$/;
const ALLOWED_DESIGNATIONS = new Set(["pr_reviewer"]);

function assertDesignationValid(designation: string): void {
  if (designation && !ALLOWED_DESIGNATIONS.has(designation)) {
    throw new ConnectError("unknown designation", Code.InvalidArgument);
  }
}

/** ADR 0057: map the proto network message (or undefined) to the stored shape. */
function normalizeNetwork(
  n: { default?: string; allowHosts?: string[]; allowHostPatterns?: string[] } | undefined,
): ProfileNetwork {
  if (!n) return { ...DEFAULT_PROFILE_NETWORK };
  return {
    default: n.default === "allow" ? "allow" : "deny",
    allowHosts: n.allowHosts ?? [],
    allowHostPatterns: n.allowHostPatterns ?? [],
  };
}

/** Map + validate proto repos to the stored shape; InvalidArgument on bad input. */
function normalizeReposChecked(
  repos: ReadonlyArray<{ path?: string; remoteUrl?: string }>,
): ReturnType<typeof normalizeRepos> {
  try {
    return normalizeRepos(repos);
  } catch (err) {
    throw new ConnectError(err instanceof Error ? err.message : "invalid repos", Code.InvalidArgument);
  }
}

/** ADR 0057: map proto secrets to the stored shape (mode coerced to broker|literal). */
function normalizeSecrets(
  secrets: ReadonlyArray<{
    ref?: string;
    envVar?: string;
    mode?: string;
    allowHosts?: string[];
    allowHostPatterns?: string[];
  }>,
): ProfileSecret[] {
  return secrets.map((s) => ({
    ref: (s.ref ?? "").trim(),
    envVar: (s.envVar ?? "").trim(),
    mode: s.mode === "literal" ? "literal" : "broker",
    allowHosts: s.allowHosts ?? [],
    allowHostPatterns: s.allowHostPatterns ?? [],
  }));
}

/** ADR 0057: a network allow entry must be a non-empty host string. */
function assertNetworkValid(n: ProfileNetwork): void {
  for (const h of [...n.allowHosts, ...n.allowHostPatterns]) {
    if (!h.trim()) {
      throw new ConnectError("network allow entry must not be empty", Code.InvalidArgument);
    }
  }
}

/**
 * ADR 0057: each profile secret needs a non-empty org-secret `ref` and a valid,
 * unique env var name. The `ref` isn't checked to exist in the org store here —
 * a secret may be entered after the profile (decoupled, like a capability vs.
 * its connector). The coordinator resolves `ref` at session create (B2).
 */
function assertSecretsValid(secrets: ProfileSecret[]): void {
  const seenEnv = new Set<string>();
  for (const s of secrets) {
    if (!s.ref) {
      throw new ConnectError(
        "secret ref (org-secret name) must not be empty",
        Code.InvalidArgument,
      );
    }
    if (!ENV_NAME_RE.test(s.envVar)) {
      throw new ConnectError(
        `invalid secret env var "${s.envVar}" (expected [A-Za-z_][A-Za-z0-9_]*)`,
        Code.InvalidArgument,
      );
    }
    if (seenEnv.has(s.envVar)) {
      throw new ConnectError(`duplicate secret env var "${s.envVar}"`, Code.InvalidArgument);
    }
    seenEnv.add(s.envVar);
  }
}

export function registerProfiles(router: ConnectRouter, deps?: ProfileDeps): void {
  const getSession: GetSession =
    deps?.getSession ??
    getSessionFromHeaders;
  const store: ProfileStore = deps?.store ?? makeProfileStore(getDb());
  const images: ImagesClient = deps?.images ?? (defaultImages as unknown as ImagesClient);
  const harnessCatalog: HarnessCatalogClient =
    deps?.harnessCatalog ?? (defaultHarnessCatalog as unknown as HarnessCatalogClient);
  const mountCatalog: MountCatalogClient = deps?.mountCatalog ?? defaultCatalog();
  // Lazy default: construct the store (and thus touch getDb()) only when a
  // handler actually reads connectors, so importing/registering without a DB
  // (tests) doesn't throw. loadRegistry degrades to built-in seeds if the read
  // fails.
  const connectors: CustomConnectorSource = deps?.connectors ?? { list: () => makeConnectorStore(getDb()).list() };
  const connections = deps?.connections ?? makeIntegrationConnectionStore(getDb());
  // Resolve the production registry lazily: ProfileService is registered before
  // startup registers all built-in tools.
  const toolCapabilities = (): Set<string> =>
    deps?.toolCapabilities ?? registeredToolCapabilities();
  const discoverRepos =
    deps?.discoverRepos ??
    ((profileId: string) => {
      const db = getDb();
      const discoverDeps: DiscoverReposDeps = {
        db,
        profiles: store,
        images,
        connectors,
        // The compile path needs the RICHER catalog view (auth env vars), so
        // the default wiring binds the production client, not the narrow
        // validation-only dep. Tests inject `discoverRepos` wholesale.
        harnessCatalog: defaultHarnessCatalog as unknown as DiscoverReposDeps["harnessCatalog"],
        connections,
        // The probe boots ownerless (programmatic credentials); user tokens
        // are never resolved.
        secrets: { get: async () => null, getAll: async () => ({}) },
        sessions: defaultSessions as unknown as DiscoverReposDeps["sessions"],
      };
      return runRepoDiscovery(discoverDeps, profileId);
    });

  /** Validate image_id against the live catalog; throw InvalidArgument if absent. */
  async function assertImageEnabled(imageId: string): Promise<void> {
    const resp = await images.listEnabledImages({});
    if (!resp.images.some((i) => i.id === imageId)) {
      throw new ConnectError("image_id is not an enabled image", Code.InvalidArgument);
    }
  }

  /**
   * ADR 0062/0063: validate a profile's harness/model/effort against the live
   * catalog. A profile ALWAYS names a concrete harness (no "inherit deployment
   * default" — superseded) that must be registered; model/effort stay optional
   * (null = the harness's own default), but a set id must exist in that harness's
   * descriptor enum. The coordinator re-checks at session-create, but failing
   * here keeps a profile from referencing something the editor wouldn't offer.
   */
  async function assertHarnessValid(
    harness: string | null,
    model: string | null,
    effort: string | null,
  ): Promise<string> {
    if (harness == null) {
      throw new ConnectError("a profile must select a harness", Code.InvalidArgument);
    }
    const { harnesses } = await harnessCatalog.listHarnesses({});
    const descriptor = harnesses.find((h) => h.name === harness)?.descriptor;
    if (!descriptor) {
      throw new ConnectError(`harness "${harness}" is not in the catalog`, Code.InvalidArgument);
    }
    if (model != null && !(descriptor.models ?? []).some((m) => m.id === model)) {
      throw new ConnectError(
        `model "${model}" is not valid for harness "${harness}"`,
        Code.InvalidArgument,
      );
    }
    if (effort != null && !(descriptor.effort ?? []).some((e) => e.id === effort)) {
      throw new ConnectError(
        `effort "${effort}" is not valid for harness "${harness}"`,
        Code.InvalidArgument,
      );
    }
    return harness;
  }

  /** Optional proto strings may arrive as ""; normalize catalog selections before validation. */
  function catalogOptionId(value: string | undefined): string | null {
    return value?.trim() || null;
  }

  /**
   * ADR 0055 P2: validate selected skills against builtins ∪ the live upload
   * catalog (like image_id). The coordinator re-checks at session-create, but
   * failing here keeps a profile from ever referencing a skill the editor
   * wouldn't have offered.
   */
  async function assertSkillsValid(skills: string[]): Promise<void> {
    if (skills.length === 0) return;
    const allowed = await selectableSkillNames(mountCatalog);
    const unknown = skills.filter((s) => !allowed.has(s));
    if (unknown.length > 0) {
      throw new ConnectError(
        `unknown skill(s): ${unknown.join(", ")}`,
        Code.InvalidArgument,
      );
    }
  }

  /**
   * ADR 0056 (B′): a capability may gate a registered built-in tool. Otherwise
   * it must be (1) a well-formed `provider:action[@resource]` string (mirrors
   * engram_core::types::Capability::parse) and (2) actually *granted* by a
   * connector — the editor offers only what a connector grants. The coordinator
   * re-validates authoritatively at session-create; rejecting here keeps a
   * profile from ever storing a grant no connector backs.
   */
  function assertResolvedGrantsValid(
    resolved: ResolvedIntegrationGrant[],
    registry: Map<string, Connector>,
    builtInToolCapabilities: Set<string>,
  ): void {
    // Profile save validates grant SHAPE only. Connection STATE (enabled,
    // endpoint membership) is enforced at session-create — editing a
    // connection auto-disables it, and that must never block unrelated edits
    // of every profile that grants it.
    validateProviderGrants(resolved);
    const capabilities = grantsToCapabilities(resolved);
    for (const c of capabilities) {
      // A named-connection capability is authorized by its CONNECTION, not by a
      // connector in the registry, so the "which connector grants this?" check
      // below does not apply to it.
      if (isProviderCapability(c)) continue;
      if (builtInToolCapabilities.has(c)) continue;
      const parsed = parseCapability(c);
      if (!parsed) {
        throw new ConnectError(
          `invalid capability "${c}": expected "provider:action[@resource]"`,
          Code.InvalidArgument,
        );
      }
      if (!grantsCapability(parsed.provider, parsed.action, registry)) {
        throw new ConnectError(
          `capability "${c}" is not granted by any connector ` +
            `(no operation grants "${parsed.provider}:${parsed.action}")`,
          Code.InvalidArgument,
        );
      }
    }

    // ADR 0115: user-scoped credential invariants.
    //  - user scope requires the connector to declare `userCredential`;
    //  - one scope per connection (the editor toggles per integration);
    //  - no user scope when two granted connections share a provider — the
    //    v1 user credential is keyed per (user, provider), so two identities
    //    of one provider would resolve the same personal token.
    const scopeByConnection = new Map<string, "org" | "user">();
    const userScopedProviders = new Map<string, string>();
    for (const { grant, connection } of resolved) {
      const scope = grant.credentialScope ?? "org";
      const prior = scopeByConnection.get(grant.connectionId);
      if (prior !== undefined && prior !== scope) {
        throw new ConnectError(
          `integration connection "${connection.alias}" mixes credential scopes; ` +
            `use one scope for all of its grants`,
          Code.InvalidArgument,
        );
      }
      scopeByConnection.set(grant.connectionId, scope);
      if (scope !== "user") continue;
      if (registry.get(connection.provider)?.userCredential === undefined) {
        throw new ConnectError(
          `connector "${connection.provider}" does not support user-scoped credentials`,
          Code.InvalidArgument,
        );
      }
      const priorConnection = userScopedProviders.get(connection.provider);
      if (priorConnection !== undefined && priorConnection !== grant.connectionId) {
        throw new ConnectError(
          `user-scoped credentials allow only one "${connection.provider}" connection per ` +
            `profile (personal credentials are keyed per provider)`,
          Code.InvalidArgument,
        );
      }
      userScopedProviders.set(connection.provider, grant.connectionId);
    }
  }

  function normalizeIntegrationGrants(
    grants: ReadonlyArray<{
      connectionId?: string;
      operation?: string;
      resourceConstraints?: string[];
      credentialScope?: string;
    }>,
  ): ProfileIntegrationGrant[] {
    return grants.map((grant) => {
      const scope = (grant.credentialScope ?? "").trim();
      if (scope !== "" && scope !== "org" && scope !== "user") {
        throw new ConnectError(
          `invalid credential scope "${scope}" (expected "org" or "user")`,
          Code.InvalidArgument,
        );
      }
      return {
        connectionId: (grant.connectionId ?? "").trim(),
        operation: (grant.operation ?? "").trim(),
        resourceConstraints: [...new Set(grant.resourceConstraints ?? [])].sort(),
        // Stored only when user-scoped; absent means org (the default).
        ...(scope === "user" ? { credentialScope: "user" as const } : {}),
      };
    });
  }

  router.service(ProfileService, {
    async listProfiles(req, ctx) {
      const user = await requireUser(ctx, getSession);
      const ability = abilityFor(user);
      if (!ability.can("read", "Profile")) throw new ConnectError("forbidden", Code.PermissionDenied);
      const isAdmin = user.role === "admin";
      // include_archived is admin-only; silently forced false for members.
      const includeArchived = isAdmin && req.includeArchived;
      const rows = await store.list({ includeArchived });
      return {
        profiles: rows.map((row) => toProto(row, isAdmin)),
      };
    },

    async getProfile(req, ctx) {
      const user = await requireUser(ctx, getSession);
      const ability = abilityFor(user);
      if (!ability.can("read", "Profile")) throw new ConnectError("forbidden", Code.PermissionDenied);
      const isAdmin = user.role === "admin";
      const row = await store.get(req.id);
      // Members never see archived profiles.
      if (!row || (row.deletedAt != null && !isAdmin)) {
        throw new ConnectError("not found", Code.NotFound);
      }
      return { profile: toProto(row, isAdmin) };
    },

    async createProfile(req, ctx) {
      const user = await requireUser(ctx, getSession);
      const ability = abilityFor(user);
      if (!ability.can("manage", "Profile")) throw new ConnectError("forbidden", Code.PermissionDenied);
      if (!req.name.trim()) throw new ConnectError("name is required", Code.InvalidArgument);
      await assertImageEnabled(req.imageId);
      const model = catalogOptionId(req.model);
      const effort = catalogOptionId(req.effort);
      const harness = await assertHarnessValid(catalogOptionId(req.harness), model, effort);
      await assertSkillsValid(req.skills ?? []);
      const integrationGrants = normalizeIntegrationGrants(req.integrationGrants ?? []);
      const resolvedGrants = await resolveIntegrationGrants(integrationGrants, connections);
      assertResolvedGrantsValid(
        resolvedGrants,
        await loadRegistry(connectors),
        toolCapabilities(),
      );
      const network = normalizeNetwork(req.network);
      const secrets = normalizeSecrets(req.secrets ?? []);
      assertNetworkValid(network);
      assertSecretsValid(secrets);
      // Validate the designation BEFORE writing the row, so a bad designation
      // rejects the whole request instead of leaving an orphan profile behind.
      if (req.designation) assertDesignationValid(req.designation);
      let row = await store.create({
        name: req.name,
        description: req.description,
        icon: req.icon || "Bot",
        imageId: req.imageId,
        includeUserTokens: req.includeUserTokens,
        envVars: req.envVars ?? {},
        skills: req.skills ?? [],
        integrationGrants,
        network,
        secrets,
        repos: normalizeReposChecked(req.repos ?? []),
        harness,
        model,
        effort,
        portExposures: req.portExposures ?? [],
      });
      if (req.designation) {
        await store.setDesignation(row.id, req.designation);
        row = (await store.get(row.id)) ?? row;
      }
      return { profile: toProto(row, true) };
    },

    async updateProfile(req, ctx) {
      const user = await requireUser(ctx, getSession);
      const ability = abilityFor(user);
      if (!ability.can("manage", "Profile")) throw new ConnectError("forbidden", Code.PermissionDenied);
      if (!req.name.trim()) throw new ConnectError("name is required", Code.InvalidArgument);
      await assertImageEnabled(req.imageId);
      const model = catalogOptionId(req.model);
      const effort = catalogOptionId(req.effort);
      const harness = await assertHarnessValid(catalogOptionId(req.harness), model, effort);
      await assertSkillsValid(req.skills ?? []);
      const integrationGrants = normalizeIntegrationGrants(req.integrationGrants ?? []);
      const resolvedGrants = await resolveIntegrationGrants(integrationGrants, connections);
      assertResolvedGrantsValid(
        resolvedGrants,
        await loadRegistry(connectors),
        toolCapabilities(),
      );
      const network = normalizeNetwork(req.network);
      const secrets = normalizeSecrets(req.secrets ?? []);
      assertNetworkValid(network);
      assertSecretsValid(secrets);
      // Validate the designation BEFORE the update commits, so a bad designation
      // rejects the request rather than half-saving the ordinary edits.
      if (req.designation !== undefined) assertDesignationValid(req.designation);
      let row = await store.update(req.id, {
        name: req.name,
        description: req.description,
        icon: req.icon || "Bot",
        imageId: req.imageId,
        includeUserTokens: req.includeUserTokens,
        envVars: req.envVars ?? {},
        skills: req.skills ?? [],
        integrationGrants,
        network,
        secrets,
        repos: normalizeReposChecked(req.repos ?? []),
        harness,
        model,
        effort,
        portExposures: req.portExposures ?? [],
      });
      if (!row) throw new ConnectError("not found", Code.NotFound);
      if (req.designation !== undefined) {
        await store.setDesignation(req.id, req.designation || null);
        row = (await store.get(req.id)) ?? row;
      }
      return { profile: toProto(row, true) };
    },

    async discoverProfileRepos(req, ctx) {
      const user = await requireUser(ctx, getSession);
      const ability = abilityFor(user);
      if (!ability.can("manage", "Profile")) throw new ConnectError("forbidden", Code.PermissionDenied);
      if (!req.profileId) throw new ConnectError("profile_id is required", Code.InvalidArgument);
      if (!(await store.getActive(req.profileId))) {
        throw new ConnectError("profile not found or archived", Code.NotFound);
      }
      let discovered: DiscoveredRepo[];
      try {
        discovered = await discoverRepos(req.profileId);
      } catch (err) {
        if (err instanceof DiscoverReposError) {
          // Boot/scan failures are environmental (no capacity, image broken) —
          // retryable, with the operator-actionable cause carried through.
          throw new ConnectError(err.message, Code.Unavailable);
        }
        throw err;
      }
      return {
        repos: discovered.map((r) => ({
          path: r.path,
          remotes: r.remotes.map((m) => ({
            name: m.name,
            url: m.url,
            parsed: m.parsed ?? undefined,
          })),
        })),
      };
    },

    async deleteProfile(req, ctx) {
      const user = await requireUser(ctx, getSession);
      const ability = abilityFor(user);
      if (!ability.can("manage", "Profile")) throw new ConnectError("forbidden", Code.PermissionDenied);
      const row = await store.get(req.id);
      if (row?.designation != null) {
        throw new ConnectError(
          `cannot delete a system profile (designation: ${row.designation})`,
          Code.FailedPrecondition,
        );
      }
      await store.softDelete(req.id);
      return {};
    },
  });
}
