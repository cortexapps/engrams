import { writeFile } from "node:fs/promises";
import { Code, ConnectError } from "@connectrpc/connect";
import { z } from "zod";
import {
  privateFile,
  requireThat,
  RunnerFailure,
  sha256,
  type Context,
  type FixtureKind,
} from "./context.ts";
import {
  jsonRequest,
  operatorClient,
  publicClient,
  type PublicClient,
} from "./transport.ts";
import type { Harness } from "./manifest.ts";

export async function waitFor<T>(
  ctx: Context,
  seam: string,
  read: () => Promise<T>,
  ready: (value: T) => boolean,
): Promise<T> {
  const deadline = performance.now() + ctx.deadlines.stepMs;
  do {
    const value = await read();
    if (ready(value)) return value;
    await new Promise((resolve) => setTimeout(resolve, 100));
  } while (performance.now() < deadline);
  throw new RunnerFailure("ASSERTION_FAILED", seam);
}
export async function bootstrapHumans(ctx: Context) {
  const clients: Partial<Record<"admin" | "owner" | "other", PublicClient>> =
    {};
  const identities: string[] = [];
  for (const role of ["admin", "owner", "other"] as const) {
    const config = ctx.identities[role];
    const password = (await privateFile(ctx.runRoot, config.passwordFile))
      .toString("utf8")
      .trim();
    requireThat(
      password.length >= 16 && password.length <= 128,
      "neutral_password_bounds",
    );
    const signup = await jsonRequest(
      ctx,
      ctx.endpoints.app,
      "POST",
      "/api/auth/sign-up/email",
      { email: config.email, password, name: `Fixture ${role}` },
      { origin: ctx.endpoints.app.origin },
    );
    requireThat(signup.status === 200, "password_signup");
    const login = await jsonRequest(
      ctx,
      ctx.endpoints.app,
      "POST",
      "/api/auth/sign-in/email",
      { email: config.email, password },
      { origin: ctx.endpoints.app.origin },
    );
    requireThat(login.status === 200, "password_login");
    const cookies = login.headers["set-cookie"];
    requireThat(cookies?.length && cookies.length <= 8, "human_session_cookie");
    const cookie = cookies.map((v) => v.split(";", 1)[0]).join("; ");
    requireThat(cookie.length <= 8192, "cookie_size");
    await writeFile(config.cookieFile, cookie, { mode: 0o600, flag: "wx" });
    const client = await publicClient(ctx, { kind: "cookie", value: cookie });
    const identity = await client.identity.whoAmI({});
    requireThat(
      identity.email === config.email &&
        !identity.serviceAccount &&
        identity.role === (role === "admin" ? "admin" : "user") &&
        identity.userId,
      "human_principal",
    );
    identities.push(identity.userId);
    clients[role] = client;
  }
  requireThat(new Set(identities).size === 3, "separate_human_principals");
  requireThat(clients.admin && clients.owner && clients.other, "human_clients");
  const key = await clients.admin.identity.createApiKey({
    name: "Fixture unattended",
    role: "user",
  });
  requireThat(
    key.key.startsWith("engk_") && key.meta?.role === "user",
    "public_api_key_creation",
  );
  await writeFile(ctx.identities.unattendedKeyFile, key.key, {
    mode: 0o600,
    flag: "wx",
  });
  const unattended = await publicClient(ctx, {
    kind: "api-key",
    value: key.key,
  });
  const identity = await unattended.identity.whoAmI({});
  requireThat(
    identity.serviceAccount &&
      identity.role === "user" &&
      !identities.includes(identity.userId),
    "unattended_principal",
  );
  const sentinel = (
    await privateFile(ctx.runRoot, ctx.identities.routerSentinelFile)
  )
    .toString("utf8")
    .trim();
  requireThat(
    /^fixture-neutral-[A-Za-z0-9_-]{16,96}$/.test(sentinel),
    "neutral_router_sentinel",
  );
  await clients.admin.secrets.putSecret({
    name: "openrouter.api_key",
    value: sentinel,
  });
  for (const client of [clients.owner, clients.other, unattended]) {
    const models = await client.models.listRouterModels({
      routerId: "openrouter",
    });
    const model = models.models.find((v) => v.id === "z-ai/glm-5.2");
    requireThat(
      model?.available && model.enabled && model.userEnabled,
      "migrated_router_model",
    );
  }
  return {
    admin: clients.admin,
    owner: clients.owner,
    other: clients.other,
    unattended,
  };
}

export async function publishOperator(ctx: Context): Promise<void> {
  const client = await operatorClient(ctx);
  const parse = (bytes: Uint8Array) => {
    try {
      const generation = z.object({
        identity: z.string().regex(/^sha256:[a-f0-9]{64}$/),
        generation: z.number().int().positive().safe(),
        candidate: z.object({
          deployment_identity: z.string(),
          network_identity: z.string(),
          trust_identity: z.string(),
        }),
      });
      return z
        .strictObject({
          admission_open: z.boolean(),
          pending: generation.nullable(),
          published: generation.nullable(),
        })
        .parse(JSON.parse(new TextDecoder().decode(bytes)));
    } catch {
      throw new RunnerFailure("ASSERTION_FAILED", "operator_publication_json");
    }
  };
  const before = await client.read({ protocol: 1 });
  requireThat(before.protocol === 1, "operator_protocol");
  const prior = parse(before.publicationJson);
  requireThat(
    prior.pending === null &&
      (prior.published?.generation ?? null) ===
        ctx.publication.expectedGeneration,
    "operator_expected_generation",
  );
  await client.stage({
    protocol: 1,
    configuredCandidateIdentity: ctx.publication.candidateIdentity,
    ...(ctx.publication.expectedGeneration === null
      ? {}
      : { expectedGeneration: BigInt(ctx.publication.expectedGeneration) }),
  });
  await waitFor(
    ctx,
    "publication_open",
    async () => {
      const response = await client.read({ protocol: 1 });
      requireThat(response.protocol === 1, "operator_protocol");
      return parse(response.publicationJson);
    },
    (value) =>
      value.pending === null &&
      value.admission_open === true &&
      value.published?.identity === ctx.publication.candidateIdentity &&
      value.published.generation ===
        (ctx.publication.expectedGeneration ?? 0) + 1 &&
      value.published.candidate?.deployment_identity ===
        ctx.publication.deploymentIdentity &&
      value.published.candidate.network_identity ===
        ctx.publication.networkIdentity &&
      value.published.candidate.trust_identity ===
        ctx.publication.trustIdentity,
  );
}

export async function createCatalog(
  ctx: Context,
  admin: PublicClient,
  suffix: string,
  protocol: FixtureKind = "modern",
) {
  requireThat(/^[a-z0-9-]{1,40}$/.test(suffix), "connection_suffix");
  const created = await admin.integration.createConnection({
    alias: `fixture-${suffix}`,
    provider: "mcp",
    displayName: "Fixture counter",
    mcp: {
      endpoint: `${ctx.protocolFixtures[protocol].resourceOrigin}/mcp`,
      zone: ctx.publication.zone,
      authKind: "api_key",
    },
  });
  requireThat(
    created.connection?.id &&
      created.connection.mcp?.endpoint ===
        `${ctx.protocolFixtures[protocol].resourceOrigin}/mcp`,
    "created_connection",
  );
  const resourceKey = (
    await privateFile(
      ctx.runRoot,
      ctx.protocolFixtures[protocol].resourceKeyFile,
    )
  )
    .toString("utf8")
    .trim();
  requireThat(
    /^[A-Za-z0-9_-]{16,128}$/.test(resourceKey),
    "neutral_resource_key",
  );
  const credential = await admin.integration.putMcpCredential({
    connectionId: created.connection.id,
    expectedRevision: created.connection.configRevision,
    expectedGeneration: created.connection.authGeneration,
    kind: "api_key",
    header: "authorization",
    value: `Bearer ${resourceKey}`,
  });
  requireThat(credential.connection, "credential_connection");
  const discovered = await admin.integration.discoverMcp({
    connectionId: created.connection.id,
    expectedRevision: credential.connection.configRevision,
  });
  requireThat(
    discovered.catalog?.tools.length === 2 &&
      new Set(discovered.catalog.tools.map((t) => t.upstreamName)).size === 2 &&
      discovered.catalog.tools.every((t) =>
        ["read_counter", "write_counter"].includes(t.upstreamName),
      ),
    "reviewed_counter_catalog",
  );
  const reviewed = discovered.catalog.tools.map((tool) => ({
    ...tool,
    classification: tool.upstreamName === "read_counter" ? "read" : "write",
    enabled: true,
  }));
  const current = await admin.integration.getMcpCatalogs({
    connectionId: created.connection.id,
  });
  requireThat(
    current.draft?.id === discovered.catalog.id,
    "exact_reviewed_draft",
  );
  const published = await admin.integration.publishMcp({
    connectionId: created.connection.id,
    draftId: discovered.catalog.id,
    expectedRevision: current.catalogRevision,
    tools: reviewed,
  });
  requireThat(
    published.catalog?.kind === "published" &&
      published.catalog.tools.length === 2,
    "published_catalog",
  );
  return { connection: credential.connection, catalog: published.catalog };
}

/** Stable compiled name from actual public connection identity; verify against public binding before prompt. */
export function modelToolName(
  connectionId: string,
  alias: string,
  upstreamName: string,
) {
  requireThat(
    /^[a-z][a-z0-9-]{1,62}$/.test(alias) &&
      connectionId.length > 0 &&
      ["read_counter", "write_counter"].includes(upstreamName),
    "actual_connection_alias",
  );
  return `mcp_${alias.replaceAll("-", "_").slice(0, 24)}_${sha256(JSON.stringify([connectionId, upstreamName])).slice(0, 20)}`;
}
export async function bindingTool(
  client: PublicClient,
  sessionId: string,
  connectionId: string,
  upstreamName: string,
) {
  let cursor = "";
  const seen = new Set<string>();
  const matches = [];
  for (let page = 0; page < 32; page++) {
    const state = await client.integration.getSessionMcpState({
      sessionId,
      omitExecutions: true,
      omitBindings: false,
      bindingPageSize: 64,
      afterBindingToolName: cursor,
    });
    requireThat(state.bindingReceipt?.current, "current_binding");
    matches.push(
      ...state.bindingTools.filter(
        (tool) =>
          tool.connectionId === connectionId &&
          tool.reviewedTool?.upstreamName === upstreamName,
      ),
    );
    if (!state.hasMoreBindingTools) {
      requireThat(matches.length === 1, "exact_public_binding");
      return matches[0]!;
    }
    requireThat(
      state.nextBindingToolName && !seen.has(state.nextBindingToolName),
      "binding_cursor",
    );
    seen.add(state.nextBindingToolName);
    cursor = state.nextBindingToolName;
  }
  throw new RunnerFailure("ASSERTION_FAILED", "binding_page_limit");
}
export async function createHumanTask(
  ctx: Context,
  owner: PublicClient,
  profileId: string,
  harness: Harness,
) {
  const response = await owner.tasks.createTask({
    type: "chat",
    profileId,
    harness,
    deferInitialPrompt: true,
    title: "Fixture acceptance",
  });
  requireThat(
    response.task?.id && response.task.sessions.length === 1,
    "fresh_task_session",
  );
  const task = await owner.tasks.updateTask({
    taskId: response.task.id,
    title: "Fixture acceptance",
  });
  requireThat(task.task?.titleIsCustom === true, "custom_title_before_prompt");
  const sessionId = response.task.sessions[0]!.sessionId;
  await waitFor(
    ctx,
    "session_started",
    () => owner.sessions.getSession({ sessionId }),
    (value) =>
      value.session?.status === "active" && value.session.hostId === ctx.hostId,
  );
  return { taskId: response.task.id, sessionId };
}
export async function expectRejected(
  action: () => Promise<unknown>,
  codes: readonly Code[],
) {
  try {
    await action();
  } catch (error) {
    requireThat(
      error instanceof ConnectError && codes.includes(error.code),
      "expected_public_rejection",
    );
    return;
  }
  throw new RunnerFailure("ASSERTION_FAILED", "unexpected_public_success");
}

export async function authorizeOAuth(
  ctx: Context,
  admin: PublicClient,
  connectionId: string,
) {
  const setup = await admin.integration.getMcpSetup({ connectionId });
  requireThat(
    setup.enabled && setup.current?.connection?.mcp,
    "oauth_public_setup",
  );
  const connection = setup.current.connection;
  const registration = setup.registrations.find(
    (r) => r.id === connection.mcp!.oauthClientRegistrationId,
  );
  requireThat(
    registration &&
      registration.redirectUri === ctx.protocolFixtures.modern.redirectUri &&
      registration.issuer === ctx.protocolFixtures.modern.issuerOrigin &&
      registration.resource?.endpoint ===
        `${ctx.protocolFixtures.modern.resourceOrigin}/mcp`,
    "oauth_exact_registration",
  );
  const begun = await admin.integration.authorizeMcp({
    connectionId,
    expectedRevision: connection.configRevision,
    expectedGeneration: connection.authGeneration,
    shareWithOrgSessions: true,
  });
  const authorization = new URL(begun.authorizationUrl);
  requireThat(
    authorization.origin === ctx.protocolFixtures.modern.authorizationOrigin &&
      authorization.pathname === "/authorize" &&
      authorization.searchParams.get("resource") ===
        `${ctx.protocolFixtures.modern.resourceOrigin}/mcp` &&
      authorization.searchParams.get("redirect_uri") ===
        registration.redirectUri &&
      authorization.searchParams.get("client_id") === registration.clientId &&
      authorization.searchParams.get("code_challenge_method") === "S256" &&
      /^[A-Za-z0-9_-]{43}$/.test(
        authorization.searchParams.get("code_challenge") ?? "",
      ),
    "oauth_exact_authorization",
  );
  const state = authorization.searchParams.get("state");
  requireThat(state && state.length <= 256, "oauth_state");
  const response = await jsonRequest(
    ctx,
    { ...ctx.protocolFixtures.modern, origin: authorization.origin },
    "GET",
    authorization.pathname + authorization.search,
  );
  requireThat(
    response.status === 302 && response.headers.location,
    "oauth_fixture_redirect",
  );
  const callback = new URL(response.headers.location);
  requireThat(
    callback.origin + callback.pathname === registration.redirectUri &&
      !callback.hash &&
      callback.searchParams.get("state") === state &&
      callback.searchParams.get("iss") === registration.issuer,
    "oauth_exact_callback",
  );
  const code = callback.searchParams.get("code");
  requireThat(code && code.length <= 512, "oauth_code");
  const completed = await admin.integration.completeMcpAuthorization({
    connectionId,
    expectedRevision: begun.configRevision,
    expectedGeneration: begun.authGeneration,
    flowId: begun.flowId,
    state,
    code,
    issuer: registration.issuer,
    redirectUri: registration.redirectUri,
  });
  requireThat(
    completed.connection?.id === connectionId &&
      completed.credential?.kind === "oauth",
    "oauth_completed",
  );
  return completed.connection;
}

export async function createFixtureProfile(
  ctx: Context,
  admin: PublicClient,
  input: {
    harness: Harness;
    suffix: string;
    connectionId: string;
    connectionAlias: string;
    upstreamName: "read_counter" | "write_counter";
    mode: "read" | "write" | "denied" | "refire" | "held-call" | "forged";
    arguments: Record<string, unknown>;
    callId: string;
    expectedNativeResult?: Record<string, unknown>;
    grant: boolean;
    automationWrite: boolean;
    constraints: object[];
    defaultAllow: boolean;
  },
) {
  requireThat(/^[a-z0-9-]{1,40}$/.test(input.suffix), "profile_suffix");
  const alias = modelToolName(
    input.connectionId,
    input.connectionAlias,
    input.upstreamName,
  );
  const proof = `${ctx.guest.stateDirectory}/${input.suffix}-{pid}.json`;
  const argumentsJson = JSON.stringify(input.arguments);
  requireThat(
    Buffer.byteLength(argumentsJson) <= 4096 &&
      /^[A-Za-z0-9_-]{1,64}$/.test(input.callId),
    "fixture_call_bounds",
  );
  const expectedJson =
    input.expectedNativeResult === undefined
      ? undefined
      : JSON.stringify(input.expectedNativeResult);
  requireThat(
    expectedJson === undefined ||
      Buffer.byteLength(expectedJson) <= 1024 * 1024,
    "expected_native_result_bounds",
  );
  const response = await admin.profiles.createProfile({
    name: `Fixture ${input.suffix}`,
    imageId: ctx.imageId,
    harness: input.harness,
    modelRouter: "openrouter",
    model: "z-ai/glm-5.2",
    expectedMcpGrantRevision: 0n,
    envVars: {
      ENGRAM_FIXTURE_TOOL: alias,
      ENGRAM_FIXTURE_MODE: input.mode,
      ENGRAM_FIXTURE_ARGUMENTS_JSON: argumentsJson,
      ENGRAM_FIXTURE_CALL_ID: input.callId,
      ...(expectedJson === undefined
        ? {}
        : { ENGRAM_FIXTURE_EXPECTED_RESULT_JSON: expectedJson }),
      ENGRAM_FIXTURE_STATE_DIR: ctx.guest.stateDirectory,
      ENGRAM_FIXTURE_PROOF: proof,
      [input.harness === "claude" ? "ENGRAM_CLAUDE_BIN" : "ENGRAM_CODEX_BIN"]:
        ctx.guest.cliPath,
    },
    network: { default: input.defaultAllow ? "allow" : "deny" },
    integrationGrants: input.grant
      ? [
          {
            connectionId: input.connectionId,
            operation: "invoke",
            credentialScope: "org",
            mcp: {
              upstreamName: input.upstreamName,
              automationWrite: input.automationWrite,
              unrestricted: input.constraints.length === 0,
              argumentConstraintDocumentsJson: input.constraints.map((v) =>
                JSON.stringify(v),
              ),
            },
          },
        ]
      : [],
  });
  requireThat(
    response.profile?.id &&
      response.profile.harness === input.harness &&
      response.profile.modelRouter === "openrouter" &&
      response.profile.model === "z-ai/glm-5.2" &&
      response.profile.mcpGrantRevision === 1n,
    "published_fixture_profile",
  );
  return { profileId: response.profile.id, alias, proof };
}
