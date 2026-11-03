import { open, realpath, readFile } from "node:fs/promises";
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
import { startSupervisor } from "./supervisor.ts";
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

export const matrixArguments = (contextPath: string) => [
  join(REPO, "orchestrator/scripts/mcp-stack/run.ts"),
  "--context",
  contextPath,
  "--matrix",
  "all",
];

async function enableGuest(
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
    job.job?.id && job.job.manifestDigest === expectedDigest,
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
    result.job?.manifestDigest === expectedDigest &&
      result.job.materializeHostId === ctx.hostId,
    "enable_actual_host",
  );
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
  let supervisor: Awaited<ReturnType<typeof startSupervisor>> | undefined;
  const signal = () => abort.abort();
  process.once("SIGINT", signal);
  process.once("SIGTERM", signal);
  let timer: ReturnType<typeof setTimeout> | undefined;
  let failure: unknown;
  try {
    const identity = await generateIdentity(inputs, root);
    engine = new Engine(inputs, root, identity.runId);
    timer = setTimeout(signal, 7200000);
    await engine.controlled(abort.signal, async () => {
      await engine!.reserveRegistryPort();
      await engine!.reserveHostPort();
      await engine!.nativeDns();
      await engine!.qualifyTools();
      const assets = await stageAssets(inputs, root, engine!);
      const configuration = await generateConfiguration(inputs, root, identity);
      stack = new Stack(inputs, root, identity, assets, configuration, engine!);
      const ctx = await stack.prepare();
      await engine!.network();
      await stack.dependencies();
      await stack.fixtures(ctx);
      await engine!.guards(identity.ca);
      await engine!.verifyNetwork(true);
      await stack.edges();
      await bootApplications(stack, ctx);
      const uri = await publishGuest(inputs, root, engine!);
      await enableGuest(ctx, identity, uri, inputs.images.guest.manifestDigest);
      ContextSchema.parse(ctx);
      const contextFile = await put(
        join(root, "context.json"),
        JSON.stringify(ctx),
      );
      await loadContext(contextFile.path);
      supervisor = await startSupervisor(ctx, {
        status: (signal) =>
          engine!.controlled(signal, () => stack!.status(ctx)),
        restart: (role, configuration, signal) =>
          engine!.controlled(signal, () =>
            stack!.restart(ctx, role, configuration),
          ),
        stop: (role, signal) =>
          engine!.controlled(signal, () => stack!.stop(role)),
      });
      await stack.status(ctx);
    });
    const contextPath = join(root, "context.json");
    const logPath = join(root, "matrix.log");
    const log = await open(logPath, "wx", 0o600);
    try {
      runner = new OwnedProcess(
        inputs.binaries.bun.path,
        matrixArguments(contextPath),
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
    await runner.stop(1500);
    runner = undefined;
  } catch (error) {
    failure = error;
  } finally {
    if (timer) clearTimeout(timer);
    process.off("SIGINT", signal);
    process.off("SIGTERM", signal);
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
    await put(
      join(root, `completion-${randomBytes(6).toString("hex")}.json`),
      JSON.stringify({
        version: 1,
        backend: inputs.backend,
        sourceCommit: inputs.sourceCommit,
        matrixCompleted: !failure,
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
    const label =
      error instanceof PlatformFailure
        ? `${error.code}:${error.seam}`
        : "FAILED:platform_composition";
    process.stderr.write(label + "\n");
    process.exitCode = 2;
  });
}
