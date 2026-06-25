/**
 * Profile → control-plane CreateSession compilation (ADR 0052/0055/0056/0057,
 * extracted in ADR 0059 P2.7).
 *
 * The intricate "turn an active profile into a CreateSession request" step —
 * image resolution, harness env (user token + CLI dummy env + profile env +
 * trigger extras), skills union, and the compiled per-session integration
 * policy. Extracted from CreateTask so the external-trigger ThreadControlPlane
 * reuses it verbatim: a triggered session runs with the SAME capabilities,
 * network, secrets, and skills as a UI task — no new privilege path.
 */

import { ConnectError, Code } from "@connectrpc/connect";

import type { ProfileRow } from "../db/profiles.ts";
import type { ImagesClient } from "./profiles.ts";
import { CLAUDE_OAUTH_ENV_VAR } from "../db/user-secrets.ts";
import {
  compileIntegrationPolicy,
  compileCliIntegrations,
  policyHasContent,
  loadRegistry,
  type CustomConnectorSource,
} from "../connectors/registry.ts";

/** The control-plane CreateSession request the orchestrator compiles. Mirrors
 *  the `SessionsClient.createSession` request shape (rpc/tasks.ts). */
export interface SessionCreateInput {
  imageUri: string;
  mode: string;
  prompt?: string;
  harnessEnv?: Record<string, string>;
  selectedSkills?: string[];
  capabilities?: string[];
  integrationPolicyJson?: string;
}

export interface SessionCompileDeps {
  images: ImagesClient;
  connectors: CustomConnectorSource;
  /** Resolve the owner's harness token for `envVar` (e.g. CLAUDE_CODE_OAUTH_TOKEN),
   *  or null. Only called when the profile sets includeUserTokens. */
  resolveUserToken: (envVar: string) => Promise<string | null>;
}

export interface SessionCompileOpts {
  prompt?: string;
  /** Extra harness env merged LAST (highest precedence) — e.g. the trigger's
   *  ENGRAM_APPEND_SYSTEM_PROMPT (ADR 0059). */
  extraHarnessEnv?: Record<string, string>;
}

/**
 * Compile a CreateSession request from an active profile. Throws
 * `FailedPrecondition` if the profile's image is no longer enabled.
 */
export async function compileSessionCreateInput(
  profile: ProfileRow,
  deps: SessionCompileDeps,
  opts: SessionCompileOpts = {},
): Promise<SessionCreateInput> {
  // Resolve image_id → current image_uri (defense in depth behind DisableImage).
  const catalog = await deps.images.listEnabledImages({});
  const image = catalog.images.find((i) => i.id === profile.imageId);
  if (!image) {
    throw new ConnectError(
      "the profile's image is no longer enabled — contact an admin",
      Code.FailedPrecondition,
    );
  }

  // Harness env, lowest → highest precedence: user token < CLI dummy env <
  // profile env_vars < trigger extras. NEVER log values.
  const harness: Record<string, string> = {};
  if (profile.includeUserTokens) {
    try {
      const userToken = await deps.resolveUserToken(CLAUDE_OAUTH_ENV_VAR);
      if (userToken) harness[CLAUDE_OAUTH_ENV_VAR] = userToken;
    } catch (secretErr) {
      console.warn("[session-compile] user token lookup failed — booting without it", secretErr);
    }
  }
  const registry = await loadRegistry(deps.connectors);
  const cliPlan = compileCliIntegrations(profile.capabilities, registry);
  for (const [k, v] of Object.entries(cliPlan.dummyEnv)) harness[k] = v;
  if (cliPlan.enabled.length > 0) harness.ENGRAM_CLI_INTEGRATIONS = JSON.stringify(cliPlan.enabled);
  for (const [k, v] of Object.entries(profile.envVars)) harness[k] = v;
  for (const [k, v] of Object.entries(opts.extraHarnessEnv ?? {})) harness[k] = v;
  const harnessEnv = Object.keys(harness).length > 0 ? harness : undefined;

  // Per-session integration policy (caps + network + secrets), shipped only
  // when it carries content.
  const policy = compileIntegrationPolicy(profile.capabilities, registry, {
    network: profile.network,
    secrets: profile.secrets,
  });
  const integrationPolicyJson = policyHasContent(policy) ? JSON.stringify(policy) : undefined;

  // Profile skills ∪ the shared integrations-cli bundle (one dyn_* slot).
  const selectedSkills = [...new Set([...profile.skills, ...cliPlan.bundles])];

  return {
    imageUri: image.imageUri,
    mode: "agent",
    ...(opts.prompt != null ? { prompt: opts.prompt } : {}),
    ...(harnessEnv != null ? { harnessEnv } : {}),
    ...(selectedSkills.length > 0 ? { selectedSkills } : {}),
    ...(profile.capabilities.length > 0 ? { capabilities: profile.capabilities } : {}),
    ...(integrationPolicyJson != null ? { integrationPolicyJson } : {}),
  };
}
