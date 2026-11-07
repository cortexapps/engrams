import { createHash } from "node:crypto";
import { lstat, open, realpath } from "node:fs/promises";
import { constants } from "node:fs";
import { dirname, isAbsolute, join, resolve, sep } from "node:path";
import { z } from "zod";
import { RunnerFailure } from "./failure.ts";

export { RunnerFailure } from "./failure.ts";
export function requireThat(
  condition: unknown,
  seam: string,
): asserts condition {
  if (!condition) throw new RunnerFailure("ASSERTION_FAILED", seam);
}
const hash = z.string().regex(/^[a-f0-9]{64}$/);
const digest = z.string().regex(/^sha256:[a-f0-9]{64}$/);
const id = z.string().regex(/^[a-zA-Z0-9_-]{1,96}$/);
const path = z.string().refine((v) => isAbsolute(v) && resolve(v) === v);
const origin = z.string().refine((v) => {
  try {
    const u = new URL(v);
    return (
      u.origin === v &&
      u.protocol === "https:" &&
      u.hostname.endsWith(".fixture.example") &&
      !u.username &&
      !u.password
    );
  } catch {
    return false;
  }
});
const file = z.strictObject({ path, sha256: hash });
const endpoint = z.strictObject({
  origin,
  ca: file,
  serverFingerprint: z.string().regex(/^(?:[A-F0-9]{2}:){31}[A-F0-9]{2}$/),
});
const fixtureEndpoint = endpoint.extend({
  configuration: file,
  runRoot: path,
  tokenFile: path,
  resourceKeyFile: path,
  resourceOrigin: origin,
  issuerOrigin: origin,
  authorizationOrigin: origin,
  tokenOrigin: origin,
  redirectUri: z.string().url(),
});
const variants = z.strictObject({
  normal: file,
  "global-disabled": file.optional(),
  "zone-disabled": file.optional(),
});
export type ConfigurationId = "normal" | "global-disabled" | "zone-disabled";
export type FixtureKind = "modern" | "legacy";
const human = z.strictObject({
  email: z.string().regex(/^[a-z0-9-]+@fixture\.example$/),
  passwordFile: path,
  cookieFile: path,
});
export const ContextSchema = z.strictObject({
  version: z.literal(1),
  runId: id,
  runRoot: path,
  backend: z.enum(["firecracker", "vz"]),
  sourceCommit: z.string().regex(/^[a-f0-9]{40}$/),
  hostId: z.uuid(),
  imageId: id,
  imageDigest: digest,
  artifacts: z.strictObject({
    coordinator: file,
    orchestrator: file,
    worker: file,
    hostAgent: file,
    fixture: file,
    kernel: file,
    guestImage: file,
    claudeHarness: file,
    codexHarness: file,
    modelCli: file,
    claudeBundle: file,
    codexBundle: file,
    helper: file,
  }),
  endpoints: z.strictObject({
    app: endpoint,
    coordinator: endpoint,
    workerControl: endpoint,
    operator: endpoint.extend({ certificate: file, privateKeyFile: path }),
  }),
  protocolFixtures: z.strictObject({
    modern: fixtureEndpoint.extend({ protocol: z.literal("2026-07-28") }),
    legacy: fixtureEndpoint.extend({
      protocol: z.enum(["2025-11-25", "2025-06-18"]),
    }),
  }),
  publication: z.strictObject({
    candidateIdentity: digest,
    expectedGeneration: z
      .number()
      .int()
      .nonnegative()
      .max(Number.MAX_SAFE_INTEGER)
      .nullable(),
    deploymentIdentity: digest,
    networkIdentity: digest,
    trustIdentity: digest,
    zone: id,
  }),
  identities: z.strictObject({
    admin: human,
    owner: human,
    other: human,
    unattendedKeyFile: path,
    automationKeyFile: path,
    routerSentinelFile: path,
  }),
  supervisor: z.strictObject({
    socket: path,
    tokenFile: path,
    configurations: z.strictObject({
      orchestrator: variants,
      worker: z.strictObject({ normal: file }),
      coordinator: z.strictObject({ normal: file }),
      "host-agent": z.strictObject({ normal: file }),
    }),
    operatorCandidates: z.strictObject({
      normal: z.strictObject({ file, identity: digest }),
    }),
  }),
  observer: z.strictObject({
    orchestratorDsnFile: path,
    coordinatorDsnFile: path,
    hostManagedRoot: path,
    hostLogFile: path,
  }),
  guest: z.strictObject({
    stateDirectory: z.literal("/tmp/mcp-stack-fixture"),
    cliPath: z.string().regex(/^\/[^\s]*\/engram-mcp-model-fixture$/),
  }),
  deadlines: z.strictObject({
    requestMs: z.number().int().min(100).max(30000),
    stepMs: z.number().int().min(100).max(120000),
    totalMs: z.number().int().min(1000).max(7200000),
  }),
  evidenceFile: path,
});
export type Context = z.infer<typeof ContextSchema>;
export type Endpoint = z.infer<typeof endpoint>;
export type Artifact = z.infer<typeof file>;
export const sha256 = (value: string | Uint8Array) =>
  createHash("sha256").update(value).digest("hex");

export async function realOwnedPath(
  root: string,
  target: string,
  absent = false,
): Promise<void> {
  if (
    !isAbsolute(root) ||
    resolve(root) !== root ||
    !isAbsolute(target) ||
    resolve(target) !== target ||
    (target !== root && !target.startsWith(root + sep))
  )
    throw new RunnerFailure("INVALID_CONTEXT", "owned_path");
  let cursor: string = sep;
  for (const part of target.split(sep).filter(Boolean)) {
    cursor = join(cursor, part);
    try {
      const stat = await lstat(cursor);
      if (stat.isSymbolicLink() || (cursor !== target && !stat.isDirectory()))
        throw new Error();
      if (cursor.startsWith(root) && stat.uid !== process.getuid?.())
        throw new Error();
    } catch (error) {
      if (
        absent &&
        cursor === target &&
        error instanceof Error &&
        "code" in error &&
        error.code === "ENOENT"
      )
        return;
      throw new RunnerFailure("INVALID_CONTEXT", "owned_path");
    }
  }
}
export async function privateFile(
  root: string,
  target: string,
  limit = 65536,
): Promise<Buffer> {
  await realOwnedPath(root, target);
  const handle = await open(target, constants.O_RDONLY | constants.O_NOFOLLOW);
  try {
    const stat = await handle.stat();
    if (
      !stat.isFile() ||
      stat.size > limit ||
      stat.size === 0 ||
      (stat.mode & 0o077) !== 0
    )
      throw new RunnerFailure("INVALID_CONTEXT", "private_file");
    return await handle.readFile();
  } finally {
    await handle.close();
  }
}
export async function verifyArtifact(
  root: string,
  artifact: Artifact,
): Promise<void> {
  await realOwnedPath(root, artifact.path);
  const handle = await open(
    artifact.path,
    constants.O_RDONLY | constants.O_NOFOLLOW,
  );
  try {
    const stat = await handle.stat();
    if (!stat.isFile() || stat.size === 0) throw new Error();
    const digest = createHash("sha256");
    for await (const chunk of handle.createReadStream({ autoClose: false }))
      digest.update(chunk);
    if (digest.digest("hex") !== artifact.sha256) throw new Error();
  } catch {
    throw new RunnerFailure("INVALID_CONTEXT", "artifact_digest");
  } finally {
    await handle.close();
  }
}
export async function loadContext(filename: string): Promise<Context> {
  // The private context is a direct child of the run root; the root never comes from an environment default.
  const root = dirname(filename);
  const raw = await privateFile(root, filename);
  let ctx: Context;
  try {
    ctx = ContextSchema.parse(JSON.parse(raw.toString("utf8")));
  } catch {
    throw new RunnerFailure("INVALID_CONTEXT", "context_schema");
  }
  if (
    ctx.runRoot !== root ||
    (await realpath(root)) !== root ||
    ((await lstat(root)).mode & 0o777) !== 0o700
  )
    throw new RunnerFailure("INVALID_CONTEXT", "private_root");
  const humanValues = Object.values({
    admin: ctx.identities.admin,
    owner: ctx.identities.owner,
    other: ctx.identities.other,
  });
  if (
    new Set(humanValues.map((v) => v.email)).size !== 3 ||
    new Set(humanValues.map((v) => v.cookieFile)).size !== 3
  )
    throw new RunnerFailure("INVALID_CONTEXT", "distinct_identities");
  const outputs = [
    ...humanValues.map((v) => v.cookieFile),
    ctx.identities.unattendedKeyFile,
    ctx.identities.automationKeyFile,
    ctx.evidenceFile,
  ];
  if (new Set(outputs).size !== outputs.length)
    throw new RunnerFailure("INVALID_CONTEXT", "distinct_outputs");
  for (const output of outputs) {
    await realOwnedPath(root, output, true);
    try {
      await lstat(output);
    } catch (e) {
      if (e instanceof Error && "code" in e && e.code === "ENOENT") continue;
      throw e;
    }
    throw new RunnerFailure("INVALID_CONTEXT", "output_exists");
  }
  for (const fixture of Object.values(ctx.protocolFixtures)) {
    await realOwnedPath(root, fixture.runRoot);
    await verifyArtifact(fixture.runRoot, fixture.configuration);
    await privateFile(fixture.runRoot, fixture.tokenFile);
    await privateFile(fixture.runRoot, fixture.resourceKeyFile);
    if (((await lstat(fixture.runRoot)).mode & 0o777) !== 0o700)
      throw new RunnerFailure("INVALID_CONTEXT", "fixture_private_root");
    if (
      new URL(fixture.redirectUri).origin !== ctx.endpoints.app.origin ||
      new URL(fixture.redirectUri).pathname !== "/integrations/mcp/callback" ||
      new URL(fixture.redirectUri).search ||
      new URL(fixture.redirectUri).hash
    )
      throw new RunnerFailure("INVALID_CONTEXT", "fixture_redirect");
  }
  if (
    ctx.protocolFixtures.modern.runRoot ===
      ctx.protocolFixtures.legacy.runRoot ||
    ctx.protocolFixtures.modern.origin === ctx.protocolFixtures.legacy.origin ||
    ctx.protocolFixtures.modern.resourceOrigin ===
      ctx.protocolFixtures.legacy.resourceOrigin
  )
    throw new RunnerFailure("INVALID_CONTEXT", "distinct_protocol_fixtures");
  if (
    ctx.deadlines.requestMs > ctx.deadlines.stepMs ||
    ctx.deadlines.stepMs > ctx.deadlines.totalMs
  )
    throw new RunnerFailure("INVALID_CONTEXT", "deadlines");
  for (const artifact of Object.values(ctx.artifacts))
    await verifyArtifact(root, artifact);
  for (const ep of [
    ...Object.values(ctx.endpoints),
    ...Object.values(ctx.protocolFixtures),
  ])
    await verifyArtifact(root, ep.ca);
  await verifyArtifact(root, ctx.endpoints.operator.certificate);
  for (const secret of [
    ...humanValues.map((v) => v.passwordFile),
    ...Object.values(ctx.protocolFixtures).flatMap((fixture) => [
      fixture.resourceKeyFile,
      fixture.tokenFile,
    ]),
    ctx.identities.routerSentinelFile,
    ctx.endpoints.operator.privateKeyFile,
    ctx.supervisor.tokenFile,
    ctx.observer.orchestratorDsnFile,
    ctx.observer.coordinatorDsnFile,
  ])
    await privateFile(root, secret);
  for (const configs of Object.values(ctx.supervisor.configurations))
    for (const artifact of Object.values(configs))
      await verifyArtifact(root, artifact);
  for (const candidate of Object.values(ctx.supervisor.operatorCandidates))
    await verifyArtifact(root, candidate.file);
  await realOwnedPath(root, ctx.observer.hostManagedRoot);
  const hostRoot = await lstat(ctx.observer.hostManagedRoot);
  if (!hostRoot.isDirectory() || (hostRoot.mode & 0o777) !== 0o700)
    throw new RunnerFailure("INVALID_CONTEXT", "host_managed_private_root");
  await realOwnedPath(root, ctx.observer.hostLogFile);
  const hostLog = await lstat(ctx.observer.hostLogFile);
  if (
    !hostLog.isFile() ||
    (hostLog.mode & 0o777) !== 0o600 ||
    hostLog.nlink !== 1
  )
    throw new RunnerFailure("INVALID_CONTEXT", "host_private_log");
  await realOwnedPath(root, ctx.supervisor.socket);
  if (
    !(await lstat(ctx.supervisor.socket)).isSocket() ||
    ((await lstat(ctx.supervisor.socket)).mode & 0o077) !== 0
  )
    throw new RunnerFailure("INVALID_CONTEXT", "supervisor_socket");
  return ctx;
}
