import { create, fromJson } from "@bufbuild/protobuf";
import { and, eq } from "drizzle-orm";
import { getDb } from "../db/client.ts";
import {
  automation,
  automationRun,
  integrationConnection,
  sessionListener,
  user,
} from "../db/schema.ts";
import { CredentialStatusSchema } from "../gen/engram/mcp/v1/control_pb.ts";
import { ManagedPolicyTargetSchema } from "../gen/engram/mcp/v1/session_policy_pb.ts";
import {
  makeCredentialControlClient,
  makeSessionPolicyControlClient,
  type SessionPolicyControlClient,
} from "./control-client.ts";
import { makeOperatorReader } from "./operator.ts";
import { makeMcpOperatorRuntime } from "./operator-runtime.ts";
import type { McpRuntimeConfiguration } from "./runtime-config.ts";
import { makeCredentialMetadataStore } from "./credential-store.ts";
import { makeCredentialAdmin } from "./credential-admin.ts";
import { makePublishedCredentialAdmin } from "./published-credentials.ts";
import { makeMcpDiscoveryAdmission } from "./discovery-admission.ts";
import { makeMcpDiscovery } from "./discovery.ts";
import { makeMcpSchemaValidator } from "./schema-validator.ts";
import { makeMcpJournal } from "./journal.ts";
import { makeMcpRepository } from "./repository.ts";
import { makeMcpRecovery } from "./recovery.ts";
import { makeMcpEffectDriver } from "./effect-driver.ts";
import { makeManagedLifecycleStore } from "./managed-lifecycle-store.ts";
import { makeManagedSessionRuntime, type ManagedBootBridge } from "./managed-runtime.ts";
import { makeManagedPolicyAuthority } from "./managed-policy.ts";
import { decodeManagedState } from "./managed-state.ts";
import { readLiveBindingAuthority, readStoredBindingSession } from "./binding-store.ts";
import { checkBindingCurrent, type McpSessionBindingSnapshot } from "./session-bindings.ts";
import { makeSessionBindingResolver } from "./session-resolver.ts";
import { makeSessionWritePolicy } from "./session-policy.ts";
import { makeMcpSessionCompiler } from "./session-compiler.ts";
import { assertMcpDirectPolicyJson } from "./session-conflicts.ts";
import { canonicalJson } from "./hash.ts";
import type { McpServices } from "./contracts.ts";
import { makeManagedRebind } from "./managed-rebind.ts";

/** A single composition is shared by RPC, compilation, execution and recovery. */
export function makeMcpRuntime(deps: {
  configuration?: McpRuntimeConfiguration;
  deploymentId: string;
  enabled(): boolean;
  bridge(control: SessionPolicyControlClient): ManagedBootBridge;
  db?: ReturnType<typeof getDb>;
  now?: () => Date;
}) {
  const db = deps.db ?? getDb(),
    now = deps.now ?? (() => new Date());
  const journal = makeMcpJournal(db),
    repository = makeMcpRepository(db);
  const store = makeManagedLifecycleStore(db);
  if (!deps.configuration)
    return {
      services: { enabled: false, journal, repository } satisfies McpServices,
      recovery: makeMcpRecovery({ db, deploymentId: deps.deploymentId, now }),
      store,
    };
  const configuration = deps.configuration;
  const control = makeSessionPolicyControlClient(configuration.control);
  const credentialsControl = makeCredentialControlClient(configuration.control);
  const bridge = deps.bridge(control);
  const reader = makeOperatorReader(control, configuration.infrastructure);
  const operator = makeMcpOperatorRuntime({
    configuration,
    read: reader.read,
    enabled: deps.enabled,
  });
  const readAuthority = async (profileId: string) =>
    readLiveBindingAuthority(profileId, await operator.policies(), db);
  async function state(sessionId: string) {
    const row = await store.read(sessionId),
      value = row.session.mcpManagedLifecycle;
    if (!value) throw Error("MCP managed creation unavailable");
    const result = decodeManagedState(
      await bridge.readManagedCreationState({
        protocol: 1,
        identity: value.identity,
        nativeVersion: value.nativeVersion,
        nativeDigest: value.nativeDigest,
      }),
      value,
    );
    return { row, value, result };
  }
  const managed = makeManagedPolicyAuthority({
    control,
    readPublished: reader.read,
    async readTarget(sessionId) {
      const { value, result } = await state(sessionId);
      const key = result.boot?.request.key;
      if (
        !key ||
        result.state === "terminal" ||
        !value.sourceKey ||
        canonicalJson(key) !== canonicalJson(value.sourceKey)
      )
        throw Error("MCP managed placement unavailable");
      return create(ManagedPolicyTargetSchema, {
        sessionId,
        sandboxId: key.context.target.sandbox_id,
        hostId: key.context.target.host_id,
        bindingEpoch: BigInt(key.context.target.binding_epoch),
      });
    },
  });
  const policy = makeSessionWritePolicy(control);
  const resolver = makeSessionBindingResolver({
    async readSession(id) {
      const { value, result } = await state(id);
      if (
        value.phase !== "started" ||
        result.state !== "boot" ||
        result.boot?.phase !== "started" ||
        canonicalJson(result.boot.request.key) !== canonicalJson(value.sourceKey)
      )
        return null;
      return readStoredBindingSession(id, db);
    },
    readAuthority,
    async readBindingEpoch(id) {
      const current = await policy.readForRequest({ sessionId: id });
      const epoch = current.ref?.bindingEpoch;
      if (
        !current.current ||
        epoch === undefined ||
        epoch < 1n ||
        epoch > BigInt(Number.MAX_SAFE_INTEGER)
      )
        throw Error("MCP binding epoch unavailable");
      return Number(epoch);
    },
    enforceManagedDeny: managed.enforce,
  });
  async function reservedSecretReferences() {
    const rows = await db
      .select({ metadata: integrationConnection.mcpCredential })
      .from(integrationConnection)
      .where(eq(integrationConnection.provider, "mcp"));
    const refs = new Set<string>();
    for (const row of rows)
      for (const raw of [row.metadata.status, row.metadata.clientSecret]) {
        if (raw) {
          const name = fromJson(CredentialStatusSchema, raw).reference?.name;
          if (name) refs.add(name);
        }
      }
    return refs;
  }
  async function validate(snapshot: McpSessionBindingSnapshot, sessionId: string) {
    const { session, owner } = await store.read(sessionId);
    if (
      ["done", "failed", "cancelled"].includes(owner.status) ||
      !checkBindingCurrent({ snapshot, liveAuthority: await readAuthority(snapshot.profileId) })
        .current
    )
      throw Error("MCP creation authority changed");
    for (const binding of snapshot.immutableBindings)
      await operator.assertCurrent(binding.config, binding.operatorPolicy);
    if (owner.spawningSessionId) {
      const parent = await resolver.readCurrent(owner.spawningSessionId),
        ref = snapshot.ancestorAuthorityRefs[0];
      if (
        !ref ||
        ref.sessionId !== owner.spawningSessionId ||
        ref.bindingHash !== parent.bindingHash ||
        ref.bindingEpoch !== parent.bindingEpoch
      )
        throw Error("MCP parent authority changed");
    } else if (snapshot.ancestorAuthorityRefs.length) throw Error("MCP parent authority missing");
    if (session.profileId !== snapshot.profileId || !owner.launchPolicy)
      throw Error("MCP launch authority unavailable");
    if (owner.launchPolicy.integrationPolicyJson)
      assertMcpDirectPolicyJson(
        owner.launchPolicy.integrationPolicyJson,
        snapshot.managedDenyPolicy.hosts,
        await reservedSecretReferences(),
      );
  }
  const lifecycle = makeManagedSessionRuntime({
    store,
    control,
    bridge,
    managed,
    readPublished: reader.read,
    validate,
    enabled: deps.enabled,
    async authorize(source, id) {
      const { session, owner } = await store.read(id);
      if (
        source.inputId !== id ||
        session.integrationPrincipalId !== source.actorId ||
        ["done", "failed", "cancelled"].includes(owner.status)
      )
        throw Error("MCP creation principal unavailable");
      if (source.actorId.startsWith("automation:")) {
        const automationId = source.actorId.slice("automation:".length);
        const rows = await db
          .select({ definition: automation, run: automationRun })
          .from(automationRun)
          .innerJoin(automation, eq(automation.id, automationRun.automationId))
          .where(
            and(
              eq(automation.id, automationId),
              eq(automationRun.taskId, owner.rootTaskId ?? owner.id),
            ),
          )
          .limit(2);
        if (
          rows.length !== 1 ||
          !rows[0]!.definition.enabled ||
          rows[0]!.definition.archivedAt ||
          rows[0]!.run.status !== "running" ||
          rows[0]!.run.dryRun ||
          rows[0]!.run.endedAt
        )
          throw Error("MCP automation creation unavailable");
        return;
      }
      const [principal] = await db.select().from(user).where(eq(user.id, source.actorId));
      if (
        !principal ||
        principal.banned ||
        source.inputId !== id ||
        session.integrationPrincipalId !== source.actorId ||
        (owner.createdByUserId !== null && owner.createdByUserId !== source.actorId) ||
        ["done", "failed", "cancelled"].includes(owner.status)
      )
        throw Error("MCP creation principal unavailable");
    },
    async enroll(sessionId) {
      await db.insert(sessionListener).values({ sessionId }).onConflictDoNothing();
    },
  });
  const driver = makeMcpEffectDriver({
    db,
    deploymentId: operator.deploymentId,
    control: credentialsControl,
    policy: control,
    resolver,
    workerTls: operator.workerTls,
    enabled: operator.enabled,
    now,
  });
  const recovery = makeMcpRecovery({
    db,
    deploymentId: deps.deploymentId,
    driver,
    cancellationPolicy: control,
    dispatchEnabled: deps.enabled,
    now,
  });
  const metadata = makeCredentialMetadataStore(db),
    admission = makeMcpDiscoveryAdmission(db);
  const rebind = makeManagedRebind({
    db,
    store,
    lifecycle,
    managed,
    readOperatorPolicies: operator.policies,
    readParentCurrent: resolver.readCurrent,
    now,
  });
  const services: McpServices = {
    get enabled() {
      return deps.enabled();
    },
    journal,
    repository,
    credentials: makePublishedCredentialAdmin({
      store: metadata,
      operator,
      admin: makeCredentialAdmin({
        store: metadata,
        control: credentialsControl,
        registrations: configuration.registrations,
        now,
      }),
    }),
    discovery: makeMcpDiscovery({
      admission,
      credentials: metadata,
      control: credentialsControl,
      operator,
      now,
    }),
    validateSchemas: makeMcpSchemaValidator({ admission, operator, now }),
    validateDestination: operator.validateDestination,
    reviewProducers: { bindings: resolver, policy: control },
    runtime: {
      cancel: async (id) => {
        recovery.abortExecution(id);
      },
      prepareRebind: rebind.prepare,
      rebind: rebind.commit,
    },
  };
  return {
    services,
    recovery,
    store,
    control,
    lifecycle,
    operator,
    managed,
    resolver,
    readAuthority,
    validate,
    capabilities: {
      compile: makeMcpSessionCompiler({
        operator,
        readAuthority,
        readParentCurrent: resolver.readCurrent,
        reservedSecretReferences,
      }),
      create: lifecycle.create,
    },
  };
}
