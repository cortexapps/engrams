import { open, realpath, readFile, readdir } from "node:fs/promises";
import { join } from "node:path";
import { networkInterfaces } from "node:os";
import { randomBytes } from "node:crypto";
import { cli, preflight, REPO } from "./preflight.ts";
import {
  check,
  childEnvironment,
  newRoot,
  OwnedProcess,
  PlatformFailure,
  put,
  readOwned,
  verifyPin,
} from "./owned.ts";
import { generateIdentity } from "./identity.ts";
import { generateConfiguration } from "./configuration.ts";
import { stageAssets, publishGuest } from "./assets.ts";
import { Engine } from "./engine.ts";
import { Stack } from "./launch.ts";
import { inspectFcInventory } from "./fc-runtime.ts";
import { parseHostRecord } from "../../../../orchestrator/scripts/mcp-stack/host-observer.ts";
import type { PublicClient } from "../../../../orchestrator/scripts/mcp-stack/transport.ts";
import { startSupervisor, type Lifecycle } from "./supervisor.ts";
import {
  ContextSchema,
  loadContext,
  type Context,
} from "../../../../orchestrator/scripts/mcp-stack/context.ts";
import {
  jsonRequest,
  publicClient,
} from "../../../../orchestrator/scripts/mcp-stack/transport.ts";
import { publicPrerequisites } from "../../../../orchestrator/scripts/mcp-stack/preflight.ts";
import { waitFor } from "../../../../orchestrator/scripts/mcp-stack/bootstrap.ts";
import type { Identity } from "./identity.ts";

import {
  EVIDENCE_MAX_BYTES,
  verifyEvidence,
} from "../../../../orchestrator/scripts/mcp-stack/evidence.ts";
import {
  FULL_MATRIX,
  selectedCases,
  type RunSelection,
} from "../../../../orchestrator/scripts/mcp-stack/manifest.ts";
import { RunnerFailure } from "../../../../orchestrator/scripts/mcp-stack/failure.ts";

export function platformFailureLabel(error: unknown): string {
  if (error instanceof PlatformFailure) return `${error.code}:${error.seam}`;
  if (error instanceof RunnerFailure) {
    switch (`${error.code}:${error.seam}`) {
      case "INVALID_CONTEXT:owned_path":
        return "FAILED:runner_invalid_context_owned_path";
      case "INVALID_CONTEXT:private_file":
        return "FAILED:runner_invalid_context_private_file";
      case "INVALID_CONTEXT:request_route":
        return "FAILED:runner_invalid_context_request_route";
      case "INVALID_CONTEXT:request_size":
        return "FAILED:runner_invalid_context_request_size";
      case "ASSERTION_FAILED:observer_token":
        return "FAILED:runner_assertion_failed_observer_token";
      case "ASSERTION_FAILED:fixture_observer_status":
        return "FAILED:runner_assertion_failed_fixture_observer_status";
      case "ASSERTION_FAILED:fixture_identity":
        return "FAILED:runner_assertion_failed_fixture_identity";
      case "TRANSPORT_FAILED:https_request":
        return "FAILED:runner_transport_failed_https_request";
    }
  }
  return "FAILED:platform_composition";
}

export const matrixArguments = (
  contextPath: string,
  selection: RunSelection = FULL_MATRIX,
) => {
  selectedCases(selection);
  return [
    join(REPO, "orchestrator/scripts/mcp-stack/run.ts"),
    "--context",
    contextPath,
    ...(selection.stage === "infrastructure"
      ? ["--stage", "infrastructure"]
      : ["--matrix", selection.matrix]),
  ];
};

/** A successful child must leave the completion record for the requested stage. */
export async function verifyRunnerCompletion(
  ctx: Context,
  selection: RunSelection,
) {
  try {
    verifyEvidence(
      ctx,
      selection,
      (await readOwned(ctx.evidenceFile, EVIDENCE_MAX_BYTES)).toString(),
    );
  } catch {
    throw new PlatformFailure("INVALID_CONTEXT", "runner_completion");
  }
}

export async function enableGuest(
  ctx: Context,
  identity: Identity,
  uri: string,
  expectedDigest: string,
) {
  const email = "bootstrap@fixture.example",
    password = (
      await readOwned(identity.secrets.bootstrapPassword.path)
    ).toString();
  const signup = await jsonRequest(
    ctx,
    ctx.endpoints.app,
    "POST",
    "/api/auth/sign-up/email",
    { email, password, name: "Fixture bootstrap" },
    { origin: ctx.endpoints.app.origin },
  );
  check(signup.status === 200, "bootstrap_signup");
  const login = await jsonRequest(
    ctx,
    ctx.endpoints.app,
    "POST",
    "/api/auth/sign-in/email",
    { email, password },
    { origin: ctx.endpoints.app.origin },
  );
  const cookies = login.headers["set-cookie"];
  check(
    login.status === 200 && cookies?.length && cookies.length <= 8,
    "bootstrap_login",
  );
  const cookie = cookies.map((v) => v.split(";", 1)[0]).join("; ");
  check(cookie.length <= 8192, "bootstrap_cookie");
  await put(join(ctx.runRoot, "bootstrap.cookie"), cookie);
  const admin = await publicClient(ctx, { kind: "cookie", value: cookie });
  const principal = await admin.identity.whoAmI({});
  check(
    principal.email === email &&
      principal.role === "admin" &&
      !principal.serviceAccount &&
      principal.userId,
    "bootstrap_identity",
  );
  const host = await waitFor(
    ctx,
    "host_enrollment",
    () => admin.fleet.getHost({ hostId: ctx.hostId }),
    (v) => v.host?.status === "ready",
  );
  check(host.host?.id === ctx.hostId, "host_enrollment_identity");
  const job = await admin.images.enableImage({
    imageUri: uri,
    config: {
      name: "Fixture guest",
      resources: {
        suggestedMemoryMib: 512,
        suggestedVcpus: 1,
        suggestedDiskGib: 2,
      },
    },
  });
  check(
    job.job?.id &&
      (job.job.manifestDigest === undefined ||
        job.job.manifestDigest === expectedDigest),
    "enable_manifest",
  );
  const result = await waitFor(
    ctx,
    "image_enable_ready",
    () => admin.images.getEnableJob({ jobId: job.job!.id }),
    (v) => {
      check(v.job?.state !== "failed", "image_enable_failed");
      return v.job?.state === "ready";
    },
  );
  check(
    result.job?.manifestDigest === undefined ||
      result.job.manifestDigest === expectedDigest,
    "enable_ready_manifest",
  );
  check(result.job?.materializeHostId === ctx.hostId, "enable_actual_host");
  const enabled = (await admin.images.listEnabledImages({})).images.filter(
    (v) => v.imageUri === uri && v.manifestDigest === expectedDigest,
  );
  check(enabled.length === 1, "enabled_image_identity");
  ctx.imageId = enabled[0]!.id;
  ctx.imageDigest = expectedDigest;
  await publicPrerequisites(ctx, admin);
  await put(
    join(ctx.runRoot, "enable-observation.json"),
    JSON.stringify({
      version: 1,
      bootstrapActorId: principal.userId,
      jobId: result.job.id,
      imageId: ctx.imageId,
      imageDigest: ctx.imageDigest,
      hostId: ctx.hostId,
      state: result.job.state,
    }),
  );
}

/** Prepare dependencies before fixture checks and application startup. */
export async function bootPlatform(
  engine: Pick<
    Engine,
    | "reserveRegistryPort"
    | "reserveHostPort"
    | "qualifyTools"
    | "network"
    | "guards"
    | "verifyNetwork"
  >,
  ca: Identity["ca"],
  prepare: () => Promise<{
    stack: Pick<
      Stack,
      | "dependencies"
      | "fixtures"
      | "edges"
      | "startRole"
      | "readyRole"
      | "migrateOrchestrator"
    >;
    ctx: Context;
  }>,
) {
  await engine.reserveRegistryPort();
  await engine.reserveHostPort();
  await engine.qualifyTools();
  const { stack, ctx } = await prepare();
  await engine.network();
  await stack.dependencies();
  await stack.fixtures(ctx);
  await engine.guards(ca);
  await engine.verifyNetwork(true);
  await stack.edges();
  await bootApplications(stack, ctx);
  return ctx;
}

/** Fixed production boot order. Migration and readiness failures stop the graph. */
export async function bootApplications(
  stack: Pick<Stack, "startRole" | "readyRole" | "migrateOrchestrator">,
  ctx: Context,
) {
  await stack.startRole(ctx, "worker", "normal");
  await stack.readyRole(ctx, "worker");
  await stack.startRole(ctx, "coordinator", "normal");
  await stack.readyRole(ctx, "coordinator");
  await stack.migrateOrchestrator();
  await stack.startRole(ctx, "orchestrator", "normal");
  await stack.readyRole(ctx, "orchestrator");
  await stack.startRole(ctx, "host-agent", "normal");
  await stack.readyRole(ctx, "host-agent");
}
export function ownedCleanupSessions(
  rows: {
    session?: { id: string; hostId?: string; status: string };
    ownerEmail?: string;
  }[],
  hostId: string,
  managed: Map<string, string>,
  actors: Set<string>,
) {
  check(rows.length <= 4096, "cleanup_session_bound");
  const seen = new Set<string>();
  const ids: string[] = [];
  for (const row of rows) {
    const session = row.session;
    check(
      session &&
        /^[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/.test(session.id) &&
        !seen.has(session.id),
      "cleanup_session_identity",
    );
    seen.add(session.id);
    if (["completed", "failed", "dead"].includes(session.status)) continue;
    check(
      (!session.hostId || session.hostId === hostId) &&
        ([
          "admin@fixture.example",
          "owner@fixture.example",
          "other@fixture.example",
          "bootstrap@fixture.example",
        ].includes(row.ownerEmail ?? "") ||
          actors.has(managed.get(session.id) ?? "")),
      "cleanup_foreign_session",
    );
    ids.push(session.id);
  }
  return ids;
}
export async function destroyRunSessions(ctx: Context, admin: PublicClient) {
  const managed = new Map<string, string>(),
    actors = new Set<string>();
  const principal = await admin.identity.whoAmI({});
  check(
    principal.email === "bootstrap@fixture.example" &&
      principal.role === "admin" &&
      !principal.serviceAccount,
    "cleanup_admin_identity",
  );
  actors.add(principal.userId);
  for (const role of ["admin", "owner", "other"] as const) {
    const file = ctx.identities[role].cookieFile;
    try {
      const client = await publicClient(ctx, {
        kind: "cookie",
        value: (await readOwned(file)).toString(),
      });
      const identity = await client.identity.whoAmI({});
      check(
        identity.email === ctx.identities[role].email &&
          !identity.serviceAccount &&
          identity.userId,
        "cleanup_fixture_identity",
      );
      actors.add(identity.userId);
    } catch (error) {
      if (
        !(
          error &&
          typeof error === "object" &&
          "code" in error &&
          error.code === "ENOENT"
        )
      )
        throw error;
    }
  }
  for (const file of [
    ctx.identities.unattendedKeyFile,
    ctx.identities.automationKeyFile,
  ]) {
    try {
      const client = await publicClient(ctx, {
        kind: "api-key",
        value: (await readOwned(file)).toString().trim(),
      });
      const identity = await client.identity.whoAmI({});
      check(identity.userId, "cleanup_key_identity");
      actors.add(identity.userId);
    } catch (error) {
      if (
        !(
          error &&
          typeof error === "object" &&
          "code" in error &&
          error.code === "ENOENT"
        )
      )
        throw error;
    }
  }
  for (const file of await readdir(ctx.observer.hostManagedRoot)) {
    if (!/^[a-f0-9-]{36}\.boot$/.test(file)) continue;
    const record = parseHostRecord(
      await readOwned(join(ctx.observer.hostManagedRoot, file)),
      file.slice(0, -5),
    ).record;
    check(
      record.request.key.context.target.host_id === ctx.hostId,
      "cleanup_managed_host",
    );
    managed.set(
      record.request.cold.session_id,
      record.request.key.creation.policy.actorId,
    );
  }
  const destroyed = new Set<string>();
  const deadline = performance.now() + 60000;
  while (performance.now() < deadline) {
    const rows = (await admin.sessions.listSessions({})).sessions;
    const ids = ownedCleanupSessions(rows, ctx.hostId, managed, actors);
    if (!ids.length) {
      await put(
        join(ctx.runRoot, "session-cleanup.json"),
        JSON.stringify({ destroyed: [...destroyed], remaining: [] }),
      );
      return;
    }
    for (const sessionId of ids) {
      await admin.sessions.deleteSession({ sessionId });
      destroyed.add(sessionId);
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new PlatformFailure("FAILED", "cleanup_session_deadline");
}
/** Write the exact runner context and retain its real control socket. */
export async function startContextSupervisor(
  ctx: Context,
  lifecycle: Lifecycle,
) {
  ContextSchema.parse(ctx);
  const contextFile = await put(
    join(ctx.runRoot, "context.json"),
    JSON.stringify(ctx),
  );
  const supervisor = await startSupervisor(ctx, lifecycle);
  try {
    await loadContext(contextFile.path);
    return supervisor;
  } catch (error) {
    await supervisor.close();
    throw error;
  }
}
/** All stages retire the same bounded owned resources, even after partial boot. */
export async function cleanupPlatform({
  context,
  stack,
  engine,
  runner,
  supervisor,
}: {
  context?: Context;
  stack?: Stack;
  engine?: Engine;
  runner?: OwnedProcess;
  supervisor?: Awaited<ReturnType<typeof startSupervisor>>;
}) {
  const failed: string[] = [];
  for (const [name, action] of [
    [
      "matrix",
      async () => {
        if (runner) await runner.stop(1500);
      },
    ],
    [
      "supervisor",
      async () => {
        if (supervisor) await supervisor.close();
      },
    ],
    [
      "sessions",
      async () => {
        if (!context) return;
        let cookie: string;
        try {
          cookie = (
            await readOwned(join(context.runRoot, "bootstrap.cookie"))
          ).toString();
        } catch (error) {
          if (
            error &&
            typeof error === "object" &&
            "code" in error &&
            error.code === "ENOENT"
          )
            return;
          throw error;
        }
        const admin = await publicClient(context, {
          kind: "cookie",
          value: cookie,
        });
        await destroyRunSessions(context, admin);
        if (engine?.fc) {
          const deadline = performance.now() + 15000;
          let released = false;
          while (performance.now() < deadline) {
            try {
              await engine.fc.requireReleased();
              released = true;
              break;
            } catch {
              await new Promise((resolve) => setTimeout(resolve, 100));
            }
          }
          check(released, "cleanup_nbd_release_deadline");
        }
      },
    ],
    [
      "host",
      async () => {
        if (stack) await stack.cleanupHost();
      },
    ],
    [
      "engine",
      async () => {
        if (engine) await engine.cleanup();
      },
    ],
  ] as const) {
    try {
      await action();
    } catch {
      failed.push(name);
    }
  }
  return failed;
}

export async function main(args: string[]) {
  const options = cli(args);
  const inputs = await preflight(
    options.backend,
    options.runRoot,
    options.manifest,
  );
  await verifyPin({
    path: await realpath(process.execPath),
    sha256: inputs.binaries.bun.sha256,
  });
  check(
    process.versions.bun ===
      (await readFile(join(REPO, ".bun-version"), "utf8")).trim(),
    "launcher_bun_version",
  );
  check(
    Object.values(networkInterfaces())
      .flat()
      .some((v) => v?.address === inputs.nativeHostIp && !v.internal),
    "native_host_address",
  );
  await newRoot(options.runRoot);
  const root = options.runRoot;
  const abort = new AbortController();
  let stack: Stack | undefined,
    engine: Engine | undefined,
    runner: OwnedProcess | undefined;
  let context: Context | undefined;
  let observing = false;
  let observer: Promise<void> | undefined;
  let observerFailure: unknown;
  let supervisor: Awaited<ReturnType<typeof startSupervisor>> | undefined;
  const signal = () => abort.abort();
  process.once("SIGINT", signal);
  process.once("SIGTERM", signal);
  let timer: ReturnType<typeof setTimeout> | undefined;
  let failure: unknown;
  try {
    const identity = await generateIdentity(inputs, root);
    engine = new Engine(
      inputs,
      root,
      identity.runId,
      inputs.backend === "firecracker"
        ? await inspectFcInventory(inputs)
        : undefined,
    );
    timer = setTimeout(signal, 7200000);
    await engine.controlled(abort.signal, async () => {
      const ctx = await bootPlatform(engine!, identity.ca, async () => {
        const assets = await stageAssets(inputs, root, engine!);
        const configuration = await generateConfiguration(
          inputs,
          root,
          identity,
        );
        stack = new Stack(
          inputs,
          root,
          identity,
          assets,
          configuration,
          engine!,
        );
        const ctx = await stack.prepare();
        context = ctx;
        return { stack, ctx };
      });
      if (engine!.fc) {
        observing = true;
        observer = (async () => {
          let firstFailure: number | undefined;
          while (observing) {
            try {
              await engine!.fc!.observe();
              firstFailure = undefined;
            } catch (error) {
              firstFailure ??= performance.now();
              if (performance.now() - firstFailure > 3000) {
                observerFailure = error;
                abort.abort();
                return;
              }
            }
            await new Promise((resolve) => setTimeout(resolve, 100));
          }
        })();
      }
      const uri = await publishGuest(inputs, root, engine!);
      await enableGuest(ctx, identity, uri, inputs.images.guest.manifestDigest);
      supervisor = await startContextSupervisor(ctx, {
        status: (signal) =>
          engine!.controlled(signal, () => stack!.status(ctx)),
        restart: (role, configuration, signal) =>
          engine!.controlled(signal, () =>
            stack!.restart(ctx, role, configuration),
          ),
        stop: (role, signal) =>
          engine!.controlled(signal, () => stack!.stop(role)),
      });
      await stack!.status(ctx);
    });
    const contextPath = join(root, "context.json");
    const logPath = join(root, "matrix.log");
    const log = await open(logPath, "wx", 0o600);
    try {
      runner = new OwnedProcess(
        inputs.binaries.bun.path,
        matrixArguments(contextPath, options.selection),
        REPO,
        childEnvironment(root),
        ["ignore", log.fd, log.fd],
      );
    } finally {
      await log.close();
    }
    const aborted = new Promise<never>((_resolve, reject) => {
      if (abort.signal.aborted)
        reject(new PlatformFailure("FAILED", "run_aborted"));
      else
        abort.signal.addEventListener(
          "abort",
          () => reject(new PlatformFailure("FAILED", "run_aborted")),
          { once: true },
        );
    });
    await Promise.race([runner.done, aborted]);
    check(runner.child.exitCode === 0, "matrix_exit");
    check(context, "runner_context");
    await verifyRunnerCompletion(context, options.selection);
    if (observerFailure) throw observerFailure;
    await engine?.fc?.observe();
    await engine?.fc?.requireCoverage();
    await runner.stop(1500);
    runner = undefined;
  } catch (error) {
    failure = error;
  } finally {
    if (timer) clearTimeout(timer);
    process.off("SIGINT", signal);
    process.off("SIGTERM", signal);
    observing = false;
    await observer;
    const failed = await cleanupPlatform({
      context,
      stack,
      engine,
      runner,
      supervisor,
    });
    await put(
      join(root, `completion-${randomBytes(6).toString("hex")}.json`),
      JSON.stringify({
        version: 1,
        backend: inputs.backend,
        sourceCommit: inputs.sourceCommit,
        ...options.selection,
        stageCompleted:
          !failure &&
          !failed.length &&
          !(
            options.selection.stage === "matrix" &&
            options.selection.matrix === "all"
          ),
        matrixCompleted:
          !failure &&
          !failed.length &&
          options.selection.stage === "matrix" &&
          options.selection.matrix === "all",
        physicalAcceptance:
          !failure &&
          !failed.length &&
          options.selection.stage === "matrix" &&
          options.selection.matrix === "all",
        cleanupFailed: failed,
      }),
    );
    if (failed.length) failure = new PlatformFailure("FAILED", "cleanup");
  }
  if (failure) throw failure;
}
if (import.meta.main) {
  main(process.argv.slice(2)).catch((error) => {
    // Never print external messages, arguments, response bodies, or credentials.
    process.stderr.write(platformFailureLabel(error) + "\n");
    process.exitCode = 2;
  });
}
