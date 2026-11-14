import { afterAll, beforeAll, expect, test } from "bun:test";
import {
  chmod,
  mkdtemp,
  mkdir,
  writeFile,
  symlink,
  rm,
  readFile,
} from "node:fs/promises";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { createServer } from "node:https";
import { X509Certificate } from "node:crypto";
import {
  ContextSchema,
  RunnerFailure,
  privateFile,
  realOwnedPath,
  sha256,
} from "./context.ts";
import {
  HARNESS,
  REQUIRED_CASES,
  selectMatrix,
  verifyResults,
  type CaseResult,
} from "./manifest.ts";
import { credentialHeaders, publicClient } from "./transport.ts";
import { createCatalog, expectRejected, modelToolName } from "./bootstrap.ts";
import { connectNodeAdapter } from "@connectrpc/connect-node";
import { Code, ConnectError } from "@connectrpc/connect";
import { ExecOutputSchema } from "../../src/gen/engram/app/v1/session_pb.ts";
import { StatusSchema } from "./supervisor.ts";
import { Evidence, verifyEvidence } from "./evidence.ts";
import { requirePrerequisites } from "./preflight.ts";
import { safeFailure, validateDrivers } from "./runner.ts";
import { caseBodyFailure, completeCaseCleanup } from "./scenario-runtime.ts";
import { drivers } from "./run.ts";
import {
  isProofFilename,
  probeOAuthCredentialAbsence,
  readGuestFile,
} from "./guest.ts";
import {
  guestColdObservation,
  probeManagedAuthority,
} from "./guest-physical.ts";
import { parseArgs } from "./cli.ts";
import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import {
  IntegrationService,
  IntegrationConnectionSchema,
  McpCatalogSchema,
  PutMcpCredentialRequestSchema,
  PublishMcpRequestSchema,
} from "../../src/gen/engram/app/v1/integration_pb.ts";
import { OperatorPublicationServiceStageRequestSchema } from "../../src/gen/engram/mcp/v1/operator_pb.ts";
import { FixtureObserver } from "./observer.ts";

let root: string;
beforeAll(async () => {
  // Resolve the repository from the source file; never use the primary checkout or OS temp directory.
  const actual = join(import.meta.dir, "../../..", "var/mcp-stack-runner");
  await realOwnedPath(actual, actual);
  root = await mkdtemp(join(actual, "unit-"));
});
afterAll(async () => {
  if (root) await rm(root, { recursive: true });
});
const h = "a".repeat(64);
const artifact = () => ({ path: "/owned/a", sha256: h });
const ep = (name: string) => ({
  origin: `https://${name}.fixture.example`,
  ca: artifact(),
  serverFingerprint: Array(32).fill("AA").join(":"),
});
function validContext() {
  return {
    version: 1,
    runId: "unit",
    runRoot: "/owned",
    backend: "vz",
    sourceCommit: "a".repeat(40),
    hostId: "11111111-1111-4111-8111-111111111111",
    imageId: "fixture-image",
    imageDigest: `sha256:${h}`,
    artifacts: Object.fromEntries(
      [
        "coordinator",
        "orchestrator",
        "worker",
        "hostAgent",
        "fixture",
        "kernel",
        "guestImage",
        "claudeHarness",
        "codexHarness",
        "modelCli",
        "claudeBundle",
        "codexBundle",
        "helper",
      ].map((key) => [key, artifact()]),
    ),
    endpoints: {
      app: ep("app"),
      coordinator: ep("coordinator"),
      workerControl: ep("worker-control"),
      operator: {
        ...ep("operator"),
        certificate: artifact(),
        privateKeyFile: "/owned/key",
      },
    },
    protocolFixtures: {
      modern: {
        ...ep("observer"),
        configuration: artifact(),
        runRoot: "/owned/modern",
        resourceKeyFile: "/owned/modern/resource.key",
        tokenFile: "/owned/token",
        resourceOrigin: ep("resource").origin,
        issuerOrigin: ep("issuer").origin,
        authorizationOrigin: ep("authorization").origin,
        tokenOrigin: ep("token").origin,
        redirectUri: ep("app").origin + "/integrations/mcp/callback",
        protocol: "2026-07-28",
      },
      legacy: {
        ...ep("observer-legacy"),
        configuration: artifact(),
        runRoot: "/owned/legacy",
        resourceKeyFile: "/owned/legacy/resource.key",
        tokenFile: "/owned/legacy/token",
        resourceOrigin: ep("resource-legacy").origin,
        issuerOrigin: ep("issuer-legacy").origin,
        authorizationOrigin: ep("authorization-legacy").origin,
        tokenOrigin: ep("token-legacy").origin,
        redirectUri: ep("app").origin + "/integrations/mcp/callback",
        protocol: "2025-11-25",
      },
    },
    publication: {
      candidateIdentity: `sha256:${h}`,
      expectedGeneration: null,
      deploymentIdentity: `sha256:${h}`,
      networkIdentity: `sha256:${h}`,
      trustIdentity: `sha256:${h}`,
      zone: "fixture",
    },
    identities: {
      ...Object.fromEntries(
        ["admin", "owner", "other"].map((role) => [
          role,
          {
            email: `${role}@fixture.example`,
            passwordFile: `/owned/${role}.password`,
            cookieFile: `/owned/${role}.cookie`,
          },
        ]),
      ),
      unattendedKeyFile: "/owned/unattended.key",
      automationKeyFile: "/owned/automation.key",
      routerSentinelFile: "/owned/router.key",
    },
    supervisor: {
      socket: "/owned/control.sock",
      tokenFile: "/owned/supervisor.token",
      configurations: Object.fromEntries(
        ["orchestrator", "worker", "coordinator", "host-agent"].map((role) => [
          role,
          { normal: artifact() },
        ]),
      ),
      operatorCandidates: {
        normal: { file: artifact(), identity: `sha256:${h}` },
      },
    },
    observer: {
      coordinatorDsnFile: "/owned/c.dsn",
      hostManagedRoot: "/owned/host/managed-boots",
      hostLogFile: "/owned/host.log",
      orchestratorDsnFile: "/owned/o.dsn",
    },
    guest: {
      stateDirectory: "/tmp/mcp-stack-fixture",
      cliPath: "/opt/fixture/engram-mcp-model-fixture",
    },
    deadlines: { requestMs: 1000, stepMs: 3000, totalMs: 10000 },
    evidenceFile: "/owned/evidence.jsonl",
  };
}
test("manifest has exactly twelve cases for each real harness", () => {
  expect(selectMatrix("all")).toHaveLength(24);
  for (const harness of HARNESS)
    expect(REQUIRED_CASES.filter((c) => c.harness === harness)).toHaveLength(
      12,
    );
  expect(() => selectMatrix("claude")).toThrow("matrix_selection");
  expect(() => selectMatrix("claude")).toThrow(RunnerFailure);
  expect(() => verifyResults([])).toThrow(RunnerFailure);
});
test("preflight rejects incomplete fixed controls before publication or signup", () => {
  const ctx = ContextSchema.parse(validContext());
  expect(() => requirePrerequisites(ctx)).toThrow(
    "guest_tls_request_deadline_6000ms",
  );
  ctx.deadlines = { requestMs: 6000, stepMs: 6000, totalMs: 10000 };
  expect(() => requirePrerequisites(ctx)).toThrow(
    "supervisor_orchestrator_global-disabled_configuration",
  );
  for (const variant of ["global-disabled", "zone-disabled"] as const) {
    ctx.supervisor.configurations.orchestrator[variant] = artifact();
  }
  expect(() => requirePrerequisites(ctx)).not.toThrow();
  ctx.supervisor.operatorCandidates.normal.identity = `sha256:${"b".repeat(64)}`;
  expect(() => requirePrerequisites(ctx)).toThrow(
    "normal_candidate_publication_identity",
  );
});
test("runner refuses missing drivers and removes raw error payloads", () => {
  expect(() => validateDrivers(drivers)).not.toThrow();
  expect(() => validateDrivers({})).toThrow("complete_driver_registry");
  const failure = safeFailure(new Error("private-token-value"));
  expect(failure.seam).toBe("unexpected_runner_error");
  expect(JSON.stringify(failure)).not.toContain("private-token-value");
});
test("acceptance proof discovery requires a fresh launch nonce, never PID-only fallback", () => {
  expect(
    isProofFilename("claude-cold", `claude-cold-12-${"a".repeat(32)}.json`),
  ).toBe(true);
  for (const name of [
    "claude-cold-12.json",
    `claude-cold-12-${"A".repeat(32)}.json`,
    `other-12-${"a".repeat(32)}.json`,
    `claude-cold-12-${"a".repeat(31)}.json`,
    `../claude-cold-12-${"a".repeat(32)}.json`,
  ])
    expect(isProofFilename("claude-cold", name)).toBe(false);
});
test("CLI requires an explicit private context and complete matrix", () => {
  expect(
    parseArgs(["--context", "/owned/context.json", "--matrix", "all"]).selected,
  ).toHaveLength(24);
  for (const args of [
    [],
    ["--context", "relative.json", "--matrix", "all"],
    ["--context", "/owned/context.json", "--matrix", "claude"],
    ["--context", "/owned/context.json", "--context", "/owned/other.json"],
    ["--context", "/owned/context.json", "--key", "secret"],
  ])
    expect(() => parseArgs(args)).toThrow();
});
test("CLI rejects a present empty stage while retaining the full default", () => {
  expect(() =>
    parseArgs(["--context", "/owned/context.json", "--stage", ""]),
  ).toThrow("arguments");
  expect(parseArgs(["--context", "/owned/context.json"]).selection).toEqual({
    stage: "matrix",
    matrix: "all",
  });
  expect(
    parseArgs(["--context", "/owned/context.json", "--matrix", "all"])
      .selection,
  ).toEqual({
    stage: "matrix",
    matrix: "all",
  });
});
test("zero and missing case results fail", () => {
  expect(() => verifyResults([])).toThrow();
  expect(() =>
    verifyResults(
      REQUIRED_CASES.map((c) => ({
        key: c.key,
        status: "NEEDS_CONTEXT",
        assertions: [],
      })),
    ),
  ).toThrow("matrix_incomplete");
});
test("every subassertion needs evidence", () => {
  const results: CaseResult[] = REQUIRED_CASES.map((c) => ({
    key: c.key,
    status: "PASS",
    assertions: c.assertions.map((assertion) => ({
      assertion,
      observationHashes: [h],
    })),
  }));
  expect(() => verifyResults(results)).not.toThrow();
  results[0]!.assertions.pop();
  expect(() => verifyResults(results)).toThrow("missing_subassertion_evidence");
});
test("duplicate case cannot replace missing case", () => {
  const results: CaseResult[] = REQUIRED_CASES.map((c) => ({
    key: c.key,
    status: "PASS",
    assertions: [],
  }));
  results[1] = results[0]!;
  expect(() => verifyResults(results)).toThrow("missing_or_duplicate_case");
});
test("strict versioned context rejects unknown and credential-bearing fields", () => {
  expect(ContextSchema.safeParse(validContext()).success).toBe(true);
  expect(
    ContextSchema.safeParse({ ...validContext(), version: 2 }).success,
  ).toBe(false);
  expect(
    ContextSchema.safeParse({ ...validContext(), apiKey: "sensitive" }).success,
  ).toBe(false);
});
test("only physical backends and fixture HTTPS origins are valid", () => {
  for (const backend of ["process", "fake", ""])
    expect(
      ContextSchema.safeParse({ ...validContext(), backend }).success,
    ).toBe(false);
  for (const origin of [
    "http://app.fixture.example",
    "https://localhost:5173",
    "https://provider.example",
    "https://app.fixture.example/",
    "https://secret@app.fixture.example",
  ]) {
    const ctx = validContext();
    ctx.endpoints.app.origin = origin;
    expect(ContextSchema.safeParse(ctx).success).toBe(false);
  }
});
test("artifact hashes and pin formats are strict", () => {
  const ctx = validContext();
  ctx.endpoints.app.serverFingerprint = h;
  expect(ContextSchema.safeParse(ctx).success).toBe(false);
  ctx.endpoints.app = ep("app");
  ctx.imageDigest = h;
  expect(ContextSchema.safeParse(ctx).success).toBe(false);
});
test("owned file rejects traversal and symlink", async () => {
  await writeFile(join(root, "secret"), "neutral", { mode: 0o600 });
  await symlink(join(root, "secret"), join(root, "link"));
  await expect(privateFile(root, join(root, "link"))).rejects.toThrow(
    "owned_path",
  );
  await expect(realOwnedPath(root, root + "/../elsewhere")).rejects.toThrow(
    "owned_path",
  );
  expect((await privateFile(root, join(root, "secret"))).toString()).toBe(
    "neutral",
  );
});
test("owned file rejects loose permissions and oversize", async () => {
  await writeFile(join(root, "loose"), "neutral", { mode: 0o644 });
  await chmod(join(root, "loose"), 0o644);
  await expect(privateFile(root, join(root, "loose"))).rejects.toThrow(
    "private_file",
  );
  await expect(privateFile(root, join(root, "secret"), 2)).rejects.toThrow(
    "private_file",
  );
});
test("identity headers cannot mix cookie and unattended principals", () => {
  expect(
    credentialHeaders({ kind: "cookie", value: "session=neutral" }),
  ).toEqual({
    cookie: "session=neutral",
  });
  expect(credentialHeaders({ kind: "api-key", value: "engk_neutral" })).toEqual(
    {
      "x-api-key": "engk_neutral",
    },
  );
  expect(() =>
    credentialHeaders({ kind: "cookie", value: "session=x\r\nx-api-key:bad" }),
  ).toThrow();
  expect(() =>
    credentialHeaders({ kind: "api-key", value: "other" }),
  ).toThrow();
});
test("compiled alias uses actual connection identity and exact upstream", () => {
  expect(modelToolName("connection-1", "fixture-one", "read_counter")).toBe(
    "mcp_fixture_one_" + sha256('["connection-1","read_counter"]').slice(0, 20),
  );
  expect(modelToolName("connection-2", "fixture-one", "read_counter")).not.toBe(
    modelToolName("connection-1", "fixture-one", "read_counter"),
  );
});
test("generated wire request preserves optimistic revisions and exact operator identity", () => {
  const stage = create(OperatorPublicationServiceStageRequestSchema, {
    protocol: 1,
    expectedGeneration: 0n,
    configuredCandidateIdentity: `sha256:${h}`,
  });
  expect(
    fromBinary(
      OperatorPublicationServiceStageRequestSchema,
      toBinary(OperatorPublicationServiceStageRequestSchema, stage),
    ),
  ).toEqual(stage);
  const request = create(PutMcpCredentialRequestSchema, {
    connectionId: "connection-1",
    expectedRevision: 7,
    expectedGeneration: 9,
    kind: "api_key",
    header: "Authorization",
    value: "Bearer neutral",
  });
  expect(
    fromBinary(
      PutMcpCredentialRequestSchema,
      toBinary(PutMcpCredentialRequestSchema, request),
    ),
  ).toEqual(request);
  const publish = create(PublishMcpRequestSchema, {
    connectionId: "connection-1",
    draftId: "draft-1",
    expectedRevision: 4,
  });
  expect(
    fromBinary(
      PublishMcpRequestSchema,
      toBinary(PublishMcpRequestSchema, publish),
    ).expectedRevision,
  ).toBe(4);
});
test("supervisor rejects arbitrary commands and missing network enforcement", () => {
  expect(
    StatusSchema.safeParse({ version: 1, command: "arbitrary" }).success,
  ).toBe(false);
  expect(
    StatusSchema.safeParse({
      version: 1,
      network: { fixtureOnlyEgress: false },
    }).success,
  ).toBe(false);
});
test("evidence refuses supplied success receipts and raw result fields", async () => {
  const ctx = ContextSchema.parse(validContext());
  ctx.evidenceFile = join(root, "evidence.jsonl");
  const evidence = await Evidence.create(ctx);
  try {
    await expect(
      evidence.result({
        key: REQUIRED_CASES[0]!.key,
        status: "PASS",
        assertions: REQUIRED_CASES[0]!.assertions.map((assertion) => ({
          assertion,
          observationHashes: [h],
        })),
      }),
    ).rejects.toThrow("case_evidence_provenance");
    await expect(
      evidence.observe({
        source: "fixture-observer",
        caseKey: REQUIRED_CASES[0]!.key,
        identitySha256: h,
        state: "succeeded",
        contentSha256: h,
        ...{ rawResult: "secret" },
      }),
    ).rejects.toThrow();
    await expect(evidence.finish()).rejects.toThrow(
      "missing_or_duplicate_case",
    );
  } finally {
    await evidence.close();
  }
});

test("native counter expectations preserve exact Codex strings and Claude metadata", async () => {
  const { expectedCounterNative, canonicalJson } = await import("./guest.ts");
  expect(expectedCounterNative("claude", 1, false)).toEqual({
    content: [{ type: "text", text: "count=1" }],
    structuredContent: { count: 1 },
    isError: false,
    _meta: {
      "engrams/toolResult": {
        outcome: "succeeded",
        delivery: "available",
        resultUnavailable: false,
        isError: false,
      },
    },
  });
  expect(expectedCounterNative("codex", 1, false)).toEqual({
    success: true,
    contentItems: [
      { type: "inputText", text: "count=1" },
      { type: "inputText", text: '{"structuredContent":{"count":1}}' },
      {
        type: "inputText",
        text: '{"delivery":"available","isError":false,"outcome":"succeeded","resultUnavailable":false}',
      },
    ],
  });
  expect(canonicalJson({ a: '{"z":1,"a":2}' })).not.toBe(
    canonicalJson({ a: '{"a":2,"z":1}' }),
  );
  expect(canonicalJson({ a: false })).not.toBe(canonicalJson({}));
  expect(() => canonicalJson({ a: undefined })).toThrow();
});

test("protocol fixtures and restart variants stay explicit", () => {
  const ctx = validContext();
  expect(ContextSchema.safeParse(ctx).success).toBe(true);
  ctx.protocolFixtures.legacy.protocol = "2026-07-28";
  expect(ContextSchema.safeParse(ctx).success).toBe(false);
  expect(
    ContextSchema.safeParse({
      ...validContext(),
      supervisor: {
        ...validContext().supervisor,
        configurations: { orchestrator: { arbitrary: artifact() } },
      },
    }).success,
  ).toBe(false);
});

test("fixed plans reject duplicate IDs, deep or large arguments and unknown fields", async () => {
  const { encodeCallPlan } = await import("./call-plan.ts");
  const step = {
    marker: "read",
    tool: "mcp_counter",
    arguments: {},
    callId: "read-1",
    resultFormat: "mcp" as const,
  };
  expect(JSON.parse(encodeCallPlan({ steps: [step] })).steps).toHaveLength(1);
  expect(() => encodeCallPlan({ steps: [step, step] })).toThrow();
  expect(() =>
    encodeCallPlan({
      steps: [{ ...step, arguments: { text: "x".repeat(4096) } }],
    }),
  ).toThrow();
  let deep: Record<string, unknown> = {};
  for (let i = 0; i < 16; i++) deep = { child: deep };
  expect(() =>
    encodeCallPlan({ steps: [{ ...step, arguments: deep }] }),
  ).toThrow();
});
test("guest JSONL preserves each flushed completion and does not invent partial output", async () => {
  const { parseGuestProofRecords } = await import("./guest.ts");
  const first = { callId: "call-1", tool: "mcp_counter", resultReceived: true };
  const second = {
    callId: "call-2",
    tool: "mcp_counter",
    resultReceived: true,
  };
  const records = parseGuestProofRecords(
    Buffer.from(JSON.stringify(first) + "\n" + JSON.stringify(second) + "\n"),
  );
  expect(records.map((value) => value.proof.callId)).toEqual([
    "call-1",
    "call-2",
  ]);
  expect(records[0]?.sha256).not.toBe(records[1]?.sha256);
  expect(parseGuestProofRecords(Buffer.from(JSON.stringify(first)))).toEqual(
    [],
  );
  expect(() => parseGuestProofRecords(Buffer.from("bad\n"))).toThrow();
});

test("automation trigger requires its own private output and cannot reuse the user key field", () => {
  const context = validContext();
  expect(ContextSchema.parse(context).identities.automationKeyFile).toBe(
    "/owned/automation.key",
  );
  const { automationKeyFile: omitted, ...identities } = context.identities;
  expect(omitted).toBeDefined();
  expect(() => ContextSchema.parse({ ...context, identities })).toThrow();
});

test("absent and premature plans cannot request fabricated expected results", async () => {
  const { encodeCallPlan } = await import("./call-plan.ts");
  const step = {
    marker: "missing",
    tool: "mcp_counter",
    arguments: {},
    callId: "missing-1",
    resultFormat: "mcp" as const,
  };
  expect(
    JSON.parse(
      encodeCallPlan({ steps: [{ ...step, expectExposure: "absent" }] }),
    ).steps[0].expectExposure,
  ).toBe("absent");
  for (const invalid of [
    { ...step, expectExposure: "absent" as const, expectedResult: {} },
    { ...step, expectExposure: "absent" as const, tool: "spawn_session" },
    {
      ...step,
      expectExposure: "absent" as const,
      resultFormat: "json" as const,
    },
    {
      ...step,
      expectExposure: "absent" as const,
      completionProjection: "premature" as const,
    },
    { ...step, completionProjection: "premature" as const, expectedResult: {} },
    {
      ...step,
      completionProjection: "premature" as const,
      resultFormat: "json" as const,
    },
  ])
    expect(() => encodeCallPlan({ steps: [invalid] })).toThrow();
});

test("native barrier and absent denial records cannot masquerade as result receipts", async () => {
  const { NativeProofSchema } = await import("./guest.ts");
  const identity = {
    callId: "call-1",
    tool: "mcp_counter",
    planSha256: `sha256:${h}`,
  };
  const held = { ...identity, observation: "completionBarrierHeld" };
  expect(NativeProofSchema.safeParse(held).success).toBe(true);
  expect(
    NativeProofSchema.safeParse({ ...held, resultReceived: true }).success,
  ).toBe(false);
  expect(
    NativeProofSchema.safeParse({ ...held, eventSha256: `sha256:${h}` })
      .success,
  ).toBe(false);
  expect(
    NativeProofSchema.safeParse({
      ...identity,
      observation: "prematureCompletionEmitted",
      eventSha256: `sha256:${h}`,
    }).success,
  ).toBe(true);
  expect(
    NativeProofSchema.safeParse({
      ...identity,
      observation: "prematureCompletionEmitted",
    }).success,
  ).toBe(false);
  const denied = {
    ...identity,
    expectExposure: "absent",
    selectedToolName: identity.tool,
    exposedMcpToolNamesSha256: `sha256:${h}`,
    requestAttempted: true,
    protocolDenialCode: "unknown_tool",
    nativeDenialCode: -32602,
  };
  expect(NativeProofSchema.safeParse(denied).success).toBe(true);
  expect(
    NativeProofSchema.safeParse({ ...denied, resultReceived: true }).success,
  ).toBe(false);
  expect(
    NativeProofSchema.safeParse({
      ...denied,
      selectedSchemaSha256: `sha256:${h}`,
    }).success,
  ).toBe(false);
  expect(
    NativeProofSchema.safeParse({ ...denied, nativeDenialCode: -32603 })
      .success,
  ).toBe(false);
});

test("grant replacement preserves public profile state and exact publication CAS", async () => {
  const { ProfileSchema, UpdateProfileRequestSchema } =
    await import("../../src/gen/engram/app/v1/profile_pb.ts");
  const { grantUpdateRequest } = await import("./rebind.ts");
  const profile = create(ProfileSchema, {
    id: "profile-1",
    name: "Fixture",
    imageId: "image-1",
    envVars: { FIXTURE: "value" },
    network: { default: "allow" },
    mcpGrantRevision: 7n,
    programmaticOnly: true,
    integrationGrants: [{ connectionId: "connection-1", operation: "invoke" }],
  });
  const request = create(
    UpdateProfileRequestSchema,
    grantUpdateRequest(profile, []),
  );
  const decoded = fromBinary(
    UpdateProfileRequestSchema,
    toBinary(UpdateProfileRequestSchema, request),
  );
  expect(decoded.expectedMcpGrantRevision).toBe(7n);
  expect(decoded.integrationGrants).toEqual([]);
  expect(decoded.envVars).toEqual({ FIXTURE: "value" });
  expect(decoded.network?.default).toBe("allow");
  expect(decoded.programmaticOnly).toBe(true);
});

test("only an exact ordered present-to-absent pair can reuse a native call ID", async () => {
  const { encodeCallPlan } = await import("./call-plan.ts");
  const present = {
    marker: "before",
    tool: "mcp_counter",
    arguments: { delta: 1 },
    callId: "same-call",
    resultFormat: "mcp" as const,
  };
  const absent = {
    ...present,
    marker: "after",
    expectExposure: "absent" as const,
  };
  expect(
    JSON.parse(encodeCallPlan({ steps: [present, absent] })).steps,
  ).toHaveLength(2);
  for (const steps of [
    [absent, present],
    [present, { ...absent, arguments: { delta: 2 } }],
    [present, { ...absent, tool: "mcp_other" }],
    [present, { ...absent, expectExposure: "present" as const }],
    [present, absent, { ...absent, marker: "third" }],
  ])
    expect(() => encodeCallPlan({ steps })).toThrow();
});

test("fixture readiness waits for transport startup and rejects invalid responses without retry", async () => {
  const keyPath = join(root, "observer.key");
  const certPath = join(root, "observer.pem");
  const certificate = Bun.spawn(
    [
      "openssl",
      "req",
      "-x509",
      "-newkey",
      "rsa:2048",
      "-nodes",
      "-keyout",
      keyPath,
      "-out",
      certPath,
      "-days",
      "1",
      "-subj",
      "/CN=localhost",
      "-addext",
      "subjectAltName=DNS:localhost",
    ],
    { stdout: "ignore", stderr: "ignore" },
  );
  expect(await certificate.exited).toBe(0);
  await chmod(certPath, 0o600);
  const cert = await readFile(certPath);
  const token = "a".repeat(48);
  const tokenPath = join(root, "observer.token");
  await writeFile(tokenPath, token, { mode: 0o600 });
  const ctx = ContextSchema.parse(validContext());
  ctx.runRoot = root;
  ctx.deadlines = { requestMs: 100, stepMs: 250, totalMs: 10000 };
  const endpoint = ctx.protocolFixtures.legacy;
  let status = 200;
  let resource = endpoint.resourceOrigin + "/mcp";
  let issuer = endpoint.issuerOrigin;
  let connections = 0;
  let transportFailures = 1;
  const requests: {
    method?: string;
    route?: string;
    authorization?: string;
  }[] = [];
  const server = createServer(
    { key: await readFile(keyPath), cert },
    (req, res) => {
      requests.push({
        method: req.method,
        route: req.url,
        authorization: req.headers.authorization,
      });
      res.writeHead(status, { "content-type": "application/json" });
      res.end(JSON.stringify({ version: 1, ready: true, resource, issuer }));
    },
  );
  server.on("connection", (socket) => {
    connections++;
    if (transportFailures-- > 0) socket.destroy();
  });
  await new Promise<void>((resolve) => server.listen(0, "localhost", resolve));
  try {
    const address = server.address();
    if (!address || typeof address === "string")
      throw new Error("server_address");
    endpoint.origin = `https://localhost:${address.port}`;
    endpoint.ca = { path: certPath, sha256: sha256(cert) };
    endpoint.serverFingerprint = new X509Certificate(cert).fingerprint256;
    endpoint.tokenFile = tokenPath;
    const observer = new FixtureObserver(ctx, "legacy");
    await observer.ready();
    expect(connections).toBe(2);
    expect(requests).toEqual([
      { method: "GET", route: "/v1/ready", authorization: `Bearer ${token}` },
    ]);

    for (const failure of ["status", "resource", "issuer", "token"] as const) {
      const before = connections;
      status = failure === "status" ? 401 : 200;
      resource =
        endpoint.resourceOrigin + (failure === "resource" ? "/wrong" : "/mcp");
      issuer = endpoint.issuerOrigin + (failure === "issuer" ? "/wrong" : "");
      await writeFile(tokenPath, failure === "token" ? "invalid" : token);
      await expect(observer.ready()).rejects.toThrow(
        failure === "status"
          ? "fixture_observer_status"
          : failure === "token"
            ? "observer_token"
            : "fixture_identity",
      );
      expect(connections - before).toBe(failure === "token" ? 0 : 1);
    }
    await writeFile(tokenPath, token);
    const beforeWrite = connections;
    transportFailures = 1;
    await expect(observer.configure({ revision: 2 })).rejects.toThrow(
      "https_request",
    );
    expect(connections - beforeWrite).toBe(1);

    for (const failure of ["transport", "certificate"] as const) {
      status = 200;
      resource = endpoint.resourceOrigin + "/mcp";
      issuer = endpoint.issuerOrigin;
      transportFailures = failure === "transport" ? 1000 : 0;
      endpoint.serverFingerprint =
        failure === "certificate"
          ? Array(32).fill("00").join(":")
          : new X509Certificate(cert).fingerprint256;
      const before = connections;
      const begun = performance.now();
      await expect(observer.ready()).rejects.toThrow(
        "TRANSPORT_FAILED:https_request",
      );
      expect(connections - before).toBeGreaterThan(1);
      expect(performance.now() - begun).toBeLessThan(1000);
    }
  } finally {
    server.closeAllConnections();
    await new Promise<void>((resolve, reject) =>
      server.close((error) => (error ? reject(error) : resolve())),
    );
  }
});

test("guest proof reads use authenticated downloads with bounded verified bytes", async () => {
  const keyPath = join(root, "download.key");
  const certPath = join(root, "download.pem");
  const certificate = Bun.spawn(
    [
      "openssl",
      "req",
      "-x509",
      "-newkey",
      "rsa:2048",
      "-nodes",
      "-keyout",
      keyPath,
      "-out",
      certPath,
      "-days",
      "1",
      "-subj",
      "/CN=localhost",
      "-addext",
      "subjectAltName=DNS:localhost",
    ],
    { stdout: "ignore", stderr: "ignore" },
  );
  expect(await certificate.exited).toBe(0);
  await chmod(certPath, 0o600);
  const cert = await readFile(certPath);
  const bytes = Buffer.from('{"callId":"proof"}\n');
  const requests: {
    url: string;
    cookie?: string;
    apiKey?: string;
    origin?: string;
  }[] = [];
  const uploads: {
    bytes: Buffer;
    size?: string;
    hash?: string;
    contentType?: string;
  }[] = [];
  let receipt: unknown;
  let status = 200;
  let rpcCode: string | string[] | undefined;
  const secret = "private-file-response-token";
  let body = bytes;
  let size = String(body.length);
  let hash = sha256(body);
  let stall = false;
  let chunked = false;
  const server = createServer(
    { key: await readFile(keyPath), cert },
    (req, res) => {
      requests.push({
        url: req.url ?? "",
        cookie: req.headers.cookie,
        apiKey: req.headers["x-api-key"]?.toString(),
        origin: req.headers.origin,
      });
      if (stall) return;
      if (req.method === "POST") {
        const chunks: Buffer[] = [];
        req.on("data", (chunk: Buffer) => chunks.push(chunk));
        req.on("end", () => {
          const uploaded = Buffer.concat(chunks);
          uploads.push({
            bytes: uploaded,
            size: req.headers["x-upload-size"]?.toString(),
            hash: req.headers["x-upload-sha256"]?.toString(),
            contentType: req.headers["content-type"],
          });
          res.writeHead(status, {
            "content-type": "application/json",
            ...(rpcCode === undefined ? {} : { "x-engram-rpc-code": rpcCode }),
            ...(status === 302 ? { location: "/redirect-target" } : {}),
          });
          res.end(
            JSON.stringify(
              receipt ?? {
                path: new URL(req.url!, "https://localhost").searchParams.get(
                  "path",
                ),
                size_bytes: uploaded.length,
                sha256: sha256(uploaded),
              },
            ),
          );
        });
        return;
      }
      res.writeHead(status, {
        "content-type": "application/octet-stream",
        ...(chunked ? {} : { "content-length": size }),
        "x-content-sha256": hash,
        "x-private": secret,
        ...(rpcCode === undefined ? {} : { "x-engram-rpc-code": rpcCode }),
        ...(status === 302 ? { location: "/redirect-target" } : {}),
      });
      res.end(body);
    },
  );
  await new Promise<void>((resolve) => server.listen(0, "localhost", resolve));
  try {
    const address = server.address();
    if (!address || typeof address === "string")
      throw new Error("server_address");
    const ctx = ContextSchema.parse(validContext());
    ctx.runRoot = root;
    ctx.endpoints.app = {
      origin: `https://localhost:${address.port}`,
      ca: { path: certPath, sha256: sha256(cert) },
      serverFingerprint: new X509Certificate(cert).fingerprint256,
    };
    ctx.deadlines.requestMs = 2000;
    const path = ctx.guest.stateDirectory + "/proof.json";
    for (const credential of [
      { kind: "cookie" as const, value: "fixture_session=owner" },
      { kind: "api-key" as const, value: "engk_owner" },
    ]) {
      const client = await publicClient(ctx, credential);
      expect(await readGuestFile(ctx, client, "owned-session", path)).toEqual(
        bytes,
      );
      expect(requests.at(-1)).toEqual({
        url: "/api/v1/sessions/owned-session/files?path=%2Ftmp%2Fmcp-stack-fixture%2Fproof.json",
        cookie: credential.kind === "cookie" ? credential.value : undefined,
        apiKey: credential.kind === "api-key" ? credential.value : undefined,
        origin: ctx.endpoints.app.origin,
      });
    }
    const fixture = Buffer.alloc(4096, 65);
    for (const credential of [
      { kind: "cookie" as const, value: "fixture_session=owner" },
      { kind: "api-key" as const, value: "engk_owner" },
    ]) {
      const uploader = await publicClient(ctx, credential);
      expect(
        await uploader.uploadGuestFile("owned-session", path, fixture),
      ).toEqual({
        sizeBytes: 4096,
        sha256: sha256(fixture),
      });
      expect(uploads.at(-1)).toEqual({
        bytes: fixture,
        size: "4096",
        hash: sha256(fixture),
        contentType: "application/octet-stream",
      });
      expect(requests.at(-1)).toMatchObject({
        cookie: credential.kind === "cookie" ? credential.value : undefined,
        apiKey: credential.kind === "api-key" ? credential.value : undefined,
        origin: ctx.endpoints.app.origin,
      });
    }
    const client = await publicClient(ctx, {
      kind: "cookie",
      value: "fixture_session=owner",
    });
    for (receipt of [
      {},
      { path, size_bytes: 4095, sha256: sha256(fixture) },
      { path, size_bytes: 4096, sha256: "0".repeat(64) },
      { path: path + "wrong", size_bytes: 4096, sha256: sha256(fixture) },
      { path, size_bytes: 4096, sha256: sha256(fixture), extra: secret },
      secret.repeat(65536),
    ]) {
      await expect(
        client.uploadGuestFile("owned-session", path, fixture),
      ).rejects.toThrow("guest_file_upload_receipt");
    }
    receipt = undefined;
    const beforeOversize = requests.length;
    await expect(
      client.uploadGuestFile(
        "owned-session",
        path,
        Buffer.alloc(1024 * 1024 + 1),
      ),
    ).rejects.toThrow("guest_file_upload_size");
    expect(requests.length).toBe(beforeOversize);
    const { anonymousGuestFileStatus } = await import("./transport.ts");
    status = 401;
    expect(await anonymousGuestFileStatus(ctx, "owned-session", path)).toBe(
      401,
    );
    expect(requests.at(-1)).toMatchObject({
      cookie: undefined,
      apiKey: undefined,
      origin: ctx.endpoints.app.origin,
    });
    status = 404;
    const nonOwner = await publicClient(ctx, {
      kind: "cookie",
      value: "fixture_session=other",
    });
    expect(await nonOwner.nonOwnerGuestFileStatus("owned-session", path)).toBe(
      404,
    );
    expect(requests.at(-1)?.cookie).toBe("fixture_session=other");
    status = 200;
    await expect(
      anonymousGuestFileStatus(ctx, "owned-session", path),
    ).rejects.toThrow("guest_file_status_http_200");
    status = 400;
    rpcCode = "3";
    for (const action of [
      () => client.uploadGuestFile("owned-session", path, fixture),
      () => anonymousGuestFileStatus(ctx, "owned-session", path),
      () => nonOwner.nonOwnerGuestFileStatus("owned-session", path),
    ]) {
      const before = requests.length;
      await expect(action()).rejects.toMatchObject({
        code: "TRANSPORT_FAILED",
        seam: "guest_file_status_http_400_rpc_code_3",
      });
      expect(requests.length).toBe(before + 1);
    }
    status = 302;
    rpcCode = undefined;
    const beforeRedirect = requests.length;
    await expect(
      client.uploadGuestFile("owned-session", path, fixture),
    ).rejects.toThrow("guest_file_status_http_302");
    await expect(
      anonymousGuestFileStatus(ctx, "owned-session", path),
    ).rejects.toThrow("guest_file_status_http_302");
    expect(requests.length).toBe(beforeRedirect + 2);
    status = 200;

    for (const invalid of [
      "../proof",
      path + "/child",
      ctx.guest.stateDirectory + "/..",
      path + "?x=1",
      path + ";id",
      path + "\n",
    ]) {
      const before = requests.length;
      await expect(
        readGuestFile(ctx, client, "owned-session", invalid),
      ).rejects.toThrow("guest_proof_path");
      expect(requests.length).toBe(before);
    }
    for (const denied of [401, 403, 404, 302]) {
      status = denied;
      const before = requests.length;
      await expect(
        readGuestFile(ctx, client, "owned-session", path),
      ).rejects.toMatchObject({
        code: "TRANSPORT_FAILED",
        seam: `guest_file_status_http_${denied}`,
      });
      expect(requests.length).toBe(before + 1);
    }
    body = Buffer.from(JSON.stringify({ error: secret }));
    size = String(body.length);
    status = 400;
    const fileErrors: Array<[string | string[] | undefined, string]> = [
      ["1", "guest_file_status_http_400_rpc_code_1"],
      ["9", "guest_file_status_http_400_rpc_code_9"],
      ["16", "guest_file_status_http_400_rpc_code_16"],
      [undefined, "guest_file_status_http_400"],
      ["", "guest_file_status_http_400"],
      ["0", "guest_file_status_http_400"],
      ["17", "guest_file_status_http_400"],
      ["09", "guest_file_status_http_400"],
      ["+9", "guest_file_status_http_400"],
      ["9junk", "guest_file_status_http_400"],
      ["9 9", "guest_file_status_http_400"],
      ["9, 9", "guest_file_status_http_400"],
      [["9", "9"], "guest_file_status_http_400"],
      [secret, "guest_file_status_http_400"],
    ];
    for (const [header, seam] of fileErrors) {
      rpcCode = header;
      for (const action of [
        () => readGuestFile(ctx, client, "owned-session", path),
        () => client.uploadGuestFile("owned-session", path, fixture),
        () => anonymousGuestFileStatus(ctx, "owned-session", path),
        () => nonOwner.nonOwnerGuestFileStatus("owned-session", path),
      ]) {
        let failure: unknown;
        try {
          await action();
        } catch (error) {
          failure = error;
        }
        expect(failure).toBeInstanceOf(RunnerFailure);
        if (!(failure instanceof RunnerFailure))
          throw new Error("expected_runner_failure");
        expect(failure.code).toBe("TRANSPORT_FAILED");
        expect(failure.seam).toBe(seam);
        expect(failure.message).toBe(`TRANSPORT_FAILED:${seam}`);
        expect(Object.keys(failure).sort()).toEqual(["code", "seam"]);
        expect(JSON.stringify(safeFailure(failure))).not.toContain(secret);
        expect(failure.stack).not.toContain(secret);
      }
    }
    status = 600;
    rpcCode = "9";
    await expect(
      readGuestFile(ctx, client, "owned-session", path),
    ).rejects.toMatchObject({
      code: "TRANSPORT_FAILED",
      seam: "guest_file_status",
    });
    status = 200;
    body = bytes;
    size = String(bytes.length);
    // Success uses file metadata and bytes, regardless of an error diagnostic header.
    expect(await readGuestFile(ctx, client, "owned-session", path)).toEqual(
      bytes,
    );
    rpcCode = undefined;
    const pin = ctx.endpoints.app.serverFingerprint;
    ctx.endpoints.app.serverFingerprint = Array(32).fill("00").join(":");
    const beforeBadPin = requests.length;
    await expect(
      readGuestFile(ctx, client, "owned-session", path),
    ).rejects.toThrow("guest_file_download");
    await expect(
      client.uploadGuestFile("owned-session", path, fixture),
    ).rejects.toThrow("guest_file_upload");
    await expect(
      anonymousGuestFileStatus(ctx, "owned-session", path),
    ).rejects.toThrow("guest_file_denial");
    expect(requests.length).toBe(beforeBadPin);
    ctx.endpoints.app.serverFingerprint = pin;
    size = String(bytes.length + 1);
    await expect(
      readGuestFile(ctx, client, "owned-session", path),
    ).rejects.toThrow("guest_file_download");
    size = String(bytes.length);
    hash = "invalid";
    await expect(
      readGuestFile(ctx, client, "owned-session", path),
    ).rejects.toThrow("guest_file_metadata");
    hash = "0".repeat(64);
    await expect(
      readGuestFile(ctx, client, "owned-session", path),
    ).rejects.toThrow("guest_file_integrity");
    hash = sha256(bytes);
    size = String(1024 * 1024 + 1);
    await expect(
      readGuestFile(ctx, client, "owned-session", path),
    ).rejects.toThrow("guest_file_metadata");
    chunked = true;
    await expect(
      readGuestFile(ctx, client, "owned-session", path),
    ).rejects.toThrow("guest_file_metadata");
    chunked = false;
    body = Buffer.alloc(1024 * 1024, 65);
    hash = sha256(body);
    size = String(body.length);
    expect(await readGuestFile(ctx, client, "owned-session", path)).toEqual(
      body,
    );
    body = Buffer.alloc(0);
    hash = sha256(body);
    size = "0";
    expect(await readGuestFile(ctx, client, "owned-session", path)).toEqual(
      body,
    );
    ctx.deadlines.requestMs = 50;
    stall = true;
    await expect(
      client.uploadGuestFile("owned-session", path, fixture),
    ).rejects.toThrow("guest_file_upload");
    await expect(
      anonymousGuestFileStatus(ctx, "owned-session", path),
    ).rejects.toThrow("guest_file_denial");
    await expect(
      readGuestFile(ctx, client, "owned-session", path),
    ).rejects.toThrow("guest_file_download");
    expect(
      requests.every((request) => request.url.startsWith("/api/v1/sessions/")),
    ).toBe(true);
  } finally {
    server.closeAllConnections();
    await new Promise<void>((resolve, reject) =>
      server.close((error) => (error ? reject(error) : resolve())),
    );
  }
});

test("public RPC diagnostics retain descriptor methods and keep error data private", async () => {
  const keyPath = join(root, "rpc-method.key");
  const certPath = join(root, "rpc-method.pem");
  const certificate = Bun.spawn(
    [
      "openssl",
      "req",
      "-x509",
      "-newkey",
      "rsa:2048",
      "-nodes",
      "-keyout",
      keyPath,
      "-out",
      certPath,
      "-days",
      "1",
      "-subj",
      "/CN=localhost",
      "-addext",
      "subjectAltName=DNS:localhost",
    ],
    { stdout: "ignore", stderr: "ignore" },
  );
  expect(await certificate.exited).toBe(0);
  await chmod(certPath, 0o600);
  const cert = await readFile(certPath);
  const secret = "private-body-message-token";
  const errorBody = { code: "failed_precondition", message: secret };
  let streamFailure = true;
  const envelope = (flags: number, bytes: Uint8Array) => {
    const header = Buffer.alloc(5);
    header[0] = flags;
    header.writeUInt32BE(bytes.length, 1);
    return Buffer.concat([header, bytes]);
  };
  const first = create(ExecOutputSchema, {
    event: { case: "started", value: { execId: "exec-fixed" } },
  });
  const server = createServer(
    { key: await readFile(keyPath), cert },
    (req, res) => {
      if (req.url === "/rpc/engram.app.v1.IntegrationService/DiscoverMcp") {
        res.writeHead(400, {
          "content-type": "application/json",
          "x-private": secret,
        });
        res.end(JSON.stringify(errorBody));
      } else if (req.url === "/rpc/engram.app.v1.SessionService/Exec") {
        res.writeHead(200, { "content-type": "application/connect+proto" });
        res.write(envelope(0, toBinary(ExecOutputSchema, first)));
        res.end(
          envelope(
            2,
            Buffer.from(
              JSON.stringify(
                streamFailure
                  ? { error: errorBody, metadata: { "x-private": [secret] } }
                  : {},
              ),
            ),
          ),
        );
      } else {
        res.writeHead(404);
        res.end();
      }
    },
  );
  await new Promise<void>((resolve) => server.listen(0, "localhost", resolve));
  try {
    const address = server.address();
    if (!address || typeof address === "string")
      throw new Error("server_address");
    const ctx = ContextSchema.parse(validContext());
    ctx.runRoot = root;
    ctx.endpoints.app = {
      origin: `https://localhost:${address.port}`,
      ca: { path: certPath, sha256: sha256(cert) },
      serverFingerprint: new X509Certificate(cert).fingerprint256,
    };
    const client = await publicClient(ctx, {
      kind: "cookie",
      value: "fixture_session=private-cookie",
    });
    let original: unknown;
    try {
      await client.integration.discoverMcp({ connectionId: "private-input" });
    } catch (error) {
      original = error;
    }
    expect(original).toBeInstanceOf(ConnectError);
    if (!(original instanceof ConnectError))
      throw new Error("expected_connect_error");
    expect(original.code).toBe(Code.FailedPrecondition);
    expect(original.rawMessage).toBe(secret);
    expect(original.metadata.get("x-private")).toBe(secret);
    expect(
      await expectRejected(
        () => Promise.reject(original),
        [Code.FailedPrecondition],
      ),
    ).toBe(Code.FailedPrecondition);
    const failure = safeFailure(original);
    expect(failure.seam).toBe(
      "public_rpc_IntegrationService.DiscoverMcp_code_9",
    );
    expect(safeFailure(original).seam).toBe(failure.seam);
    expect(original.rawMessage).toBe(secret);
    expect(Object.keys(original).sort()).toEqual(
      Object.keys(new ConnectError(secret, Code.FailedPrecondition)).sort(),
    );
    expect(JSON.stringify(failure)).not.toContain("private");
    expect(
      safeFailure(new ConnectError(secret, Code.FailedPrecondition)).seam,
    ).toBe("public_rpc_code_9");
    expect(safeFailure(new Error(secret)).seam).toBe("unexpected_runner_error");
    const stream = client.sessions
      .exec({ sessionId: "private-session", argv: ["private-arg"] })
      [Symbol.asyncIterator]();
    expect((await stream.next()).value).toEqual(first);
    let deferred: unknown;
    try {
      await stream.next();
    } catch (error) {
      deferred = error;
    }
    expect(deferred).toBeInstanceOf(ConnectError);
    if (!(deferred instanceof ConnectError))
      throw new Error("expected_stream_error");
    expect(deferred.rawMessage).toBe(secret);
    expect(deferred.metadata.get("x-private")).toBe(secret);
    expect(safeFailure(deferred).seam).toBe(
      "public_rpc_SessionService.Exec_code_9",
    );
    expect(JSON.stringify(safeFailure(deferred))).not.toContain("private");
    streamFailure = false;
    const complete = [];
    for await (const message of client.sessions.exec({ sessionId: "session" }))
      complete.push(message);
    expect(complete).toEqual([first]);
  } finally {
    server.closeAllConnections();
    await new Promise<void>((resolve, reject) =>
      server.close((error) => (error ? reject(error) : resolve())),
    );
  }
});

test("catalog bootstrap enables the published connection with the authorized revision and checks the response", async () => {
  const keyPath = join(root, "catalog.key");
  const certPath = join(root, "catalog.pem");
  const certificate = Bun.spawn(
    [
      "openssl",
      "req",
      "-x509",
      "-newkey",
      "rsa:2048",
      "-nodes",
      "-keyout",
      keyPath,
      "-out",
      certPath,
      "-days",
      "1",
      "-subj",
      "/CN=localhost",
      "-addext",
      "subjectAltName=DNS:localhost",
    ],
    { stdout: "ignore", stderr: "ignore" },
  );
  expect(await certificate.exited).toBe(0);
  await chmod(certPath, 0o600);
  const cert = await readFile(certPath);
  const ctx = ContextSchema.parse(validContext());
  ctx.runRoot = root;
  ctx.protocolFixtures.modern.resourceKeyFile = join(
    root,
    "catalog-resource.key",
  );
  await writeFile(
    ctx.protocolFixtures.modern.resourceKeyFile,
    "fixture-neutral-resource-key",
    {
      mode: 0o600,
    },
  );
  const created = create(IntegrationConnectionSchema, {
    id: "connection-1",
    alias: "fixture-catalog",
    provider: "mcp",
    enabled: false,
    configRevision: 3,
    authGeneration: 5,
    catalogRevision: 0,
    mcp: {
      endpoint: ctx.protocolFixtures.modern.resourceOrigin + "/mcp",
      zone: "fixture",
      authKind: "api_key",
    },
  });
  const authorized = create(IntegrationConnectionSchema, {
    ...created,
    configRevision: 7,
    authGeneration: 6,
    credentialIdentity: "credential-1",
  });
  const draft = create(McpCatalogSchema, {
    id: "draft-1",
    connectionId: "connection-1",
    kind: "draft",
    revision: 11,
    configRevision: 7,
    authGeneration: 6,
    tools: ["read_counter", "write_counter"].map((upstreamName) => ({
      upstreamName,
      inputSchemaJson: '{"type":"object"}',
    })),
  });
  const published = create(McpCatalogSchema, {
    ...draft,
    id: "published-1",
    kind: "published",
    revision: 12,
  });
  const enabled = create(IntegrationConnectionSchema, {
    ...authorized,
    enabled: true,
    catalogRevision: 12,
    testedAt: "2026-01-01T00:00:00Z",
  });
  let mode = "success";
  const calls: string[] = [];
  const handler = connectNodeAdapter({
    requestPathPrefix: "/rpc",
    routes(router) {
      router.service(IntegrationService, {
        createConnection(req, rpc) {
          calls.push("create");
          expect(rpc.requestHeader.get("cookie")).toBe("fixture_session=admin");
          expect(rpc.requestHeader.has("x-api-key")).toBe(false);
          expect(req.alias).toBe("fixture-catalog");
          return { connection: created };
        },
        putMcpCredential(req) {
          calls.push("authorize");
          expect(req.connectionId).toBe("connection-1");
          expect(req.expectedRevision).toBe(3);
          expect(req.expectedGeneration).toBe(5);
          return { connection: authorized };
        },
        discoverMcp(req) {
          calls.push("discover");
          expect(req.connectionId).toBe("connection-1");
          expect(req.expectedRevision).toBe(7);
          return { catalog: draft };
        },
        getMcpCatalogs(req) {
          calls.push("review");
          expect(req.connectionId).toBe("connection-1");
          return { draft, configRevision: 7, catalogRevision: 11 };
        },
        publishMcp(req) {
          calls.push("publish");
          expect(req.connectionId).toBe("connection-1");
          expect(req.draftId).toBe("draft-1");
          expect(req.expectedRevision).toBe(11);
          expect(
            req.tools.map((tool) => [
              tool.upstreamName,
              tool.classification,
              tool.enabled,
            ]),
          ).toEqual([
            ["read_counter", "read", true],
            ["write_counter", "write", true],
          ]);
          return { catalog: published };
        },
        setConnectionEnabled(req, rpc) {
          calls.push("enable");
          expect(calls).toEqual([
            "create",
            "authorize",
            "discover",
            "review",
            "publish",
            "enable",
          ]);
          expect(rpc.requestHeader.get("cookie")).toBe("fixture_session=admin");
          expect(rpc.requestHeader.has("x-api-key")).toBe(false);
          expect(req.id).toBe("connection-1");
          expect(req.expectedRevision).toBe(7);
          expect(req.enabled).toBe(true);
          if (mode === "rpc-failure")
            throw new ConnectError("enable refused", Code.FailedPrecondition);
          if (mode === "missing") return {};
          return {
            connection: create(IntegrationConnectionSchema, {
              ...enabled,
              ...(mode === "wrong-id" ? { id: "connection-2" } : {}),
              ...(mode === "wrong-revision" ? { configRevision: 8 } : {}),
              ...(mode === "disabled" ? { enabled: false } : {}),
            }),
          };
        },
      });
    },
  });
  const server = createServer({ key: await readFile(keyPath), cert }, handler);
  await new Promise<void>((resolve) => server.listen(0, "localhost", resolve));
  try {
    const address = server.address();
    if (!address || typeof address === "string")
      throw new Error("server_address");
    ctx.endpoints.app = {
      origin: `https://localhost:${address.port}`,
      ca: { path: certPath, sha256: sha256(cert) },
      serverFingerprint: new X509Certificate(cert).fingerprint256,
    };
    const client = await publicClient(ctx, {
      kind: "cookie",
      value: "fixture_session=admin",
    });
    const result = await createCatalog(ctx, client, "catalog");
    expect(calls).toEqual([
      "create",
      "authorize",
      "discover",
      "review",
      "publish",
      "enable",
    ]);
    expect(result.connection).toEqual(enabled);
    expect(result.catalog).toEqual(published);
    for (mode of [
      "rpc-failure",
      "wrong-id",
      "wrong-revision",
      "disabled",
      "missing",
    ]) {
      calls.length = 0;
      await expect(createCatalog(ctx, client, "catalog")).rejects.toThrow(
        mode === "rpc-failure" ? "enable refused" : "enabled_connection",
      );
      expect(calls).toEqual([
        "create",
        "authorize",
        "discover",
        "review",
        "publish",
        "enable",
      ]);
    }
  } finally {
    server.closeAllConnections();
    await new Promise<void>((resolve, reject) =>
      server.close((error) => (error ? reject(error) : resolve())),
    );
  }
});

test("matrix selectors reject empty, unknown and duplicate cases and retain canonical order", () => {
  const first = REQUIRED_CASES[0]!,
    last = REQUIRED_CASES.at(-1)!;
  expect(selectMatrix(`${last.key},${first.key}`)).toEqual([first, last]);
  for (const value of [
    "",
    "claude",
    "all,all",
    `${first.key},`,
    `,${first.key}`,
    `${first.key},${first.key}`,
    ` ${first.key}`,
    "claude/unknown",
  ])
    expect(() => selectMatrix(value)).toThrow("matrix_selection");
  expect(selectMatrix(REQUIRED_CASES.map((c) => c.key).join(","))).toEqual(
    REQUIRED_CASES,
  );
  expect(parseArgs(["--context", "/owned/context.json"]).selection).toEqual({
    stage: "matrix",
    matrix: "all",
  });
  expect(
    parseArgs(["--context", "/owned/context.json", "--stage", "infrastructure"])
      .selection,
  ).toEqual({ stage: "infrastructure" });
  expect(
    parseArgs(["--context", "/owned/context.json", "--matrix", first.key])
      .selected,
  ).toEqual([first]);
  for (const args of [
    ["--context", "/owned/context.json", "--stage", "other"],
    [
      "--context",
      "/owned/context.json",
      "--stage",
      "infrastructure",
      "--matrix",
      "all",
    ],
  ])
    expect(() => parseArgs(args)).toThrow();
});

const infrastructureRows = () =>
  (["ordinary", "managed"] as const).map((mode) => ({
    mode,
    execSha256: h,
    uploadSha256: h,
    downloadSha256: h,
    sizeBytes: 32,
    unauthenticatedStatus: 401 as const,
    nonOwnerStatus: 404 as const,
  }));
for (const selection of [
  { stage: "infrastructure" },
  { stage: "matrix", matrix: REQUIRED_CASES[0]!.key },
  { stage: "matrix", matrix: "all" },
] as const)
  for (const mode of ["writer", "replay"] as const)
    test(`evidence ${mode} requires prerequisite provenance for ${selection.stage === "matrix" ? selection.matrix : selection.stage}`, async () => {
      const ctx = ContextSchema.parse(validContext());
      ctx.evidenceFile = join(
        root,
        `prerequisites-${mode}-${selection.stage === "matrix" ? selection.matrix.replaceAll("/", "-") : selection.stage}.jsonl`,
      );
      const evidence = await Evidence.create(ctx, selection);
      try {
        const cases =
          selection.stage === "matrix" ? selectMatrix(selection.matrix) : [];
        const full = selection.stage === "matrix" && selection.matrix === "all";
        await recordCases(evidence, cases);
        await evidence.infrastructure(infrastructureRows());
        if (mode === "writer") {
          await expect(evidence.finish()).rejects.toThrow(
            "prerequisites_evidence_required",
          );
        } else {
          const source = await readFile(ctx.evidenceFile, "utf8");
          const terminal = {
            version: 1,
            kind: full ? "complete" : "stage-complete",
            ...selection,
            selected: cases.length,
            passed: cases.length,
            stageCompleted: !full,
            matrixCompleted: full,
            physicalAcceptance: full,
          };
          expect(() =>
            verifyEvidence(
              ctx,
              selection,
              source + JSON.stringify(terminal) + "\n",
            ),
          ).toThrow("prerequisites_evidence_required");
        }
        await recordPrerequisites(evidence);
        await expect(recordPrerequisites(evidence)).rejects.toThrow(
          "prerequisites_evidence_duplicate",
        );
        await evidence.finish();
        const source = await readFile(ctx.evidenceFile, "utf8");
        expect(() => verifyEvidence(ctx, selection, source)).not.toThrow();
        const prerequisite = source
          .split("\n")
          .find((line) => line.includes('"kind":"prerequisites"'))!;
        expect(() =>
          verifyEvidence(
            ctx,
            selection,
            source.replace(
              prerequisite + "\n",
              prerequisite + "\n" + prerequisite + "\n",
            ),
          ),
        ).toThrow("prerequisites_evidence_duplicate");
      } finally {
        await evidence.close();
      }
    });
async function recordPrerequisites(evidence: Evidence) {
  await evidence.prerequisites({
    supervisorSha256: h,
    publicationSha256: h,
    publicStateSha256: h,
  });
}
async function recordCases(evidence: Evidence, cases = REQUIRED_CASES) {
  for (const selected of cases) {
    const digest = await evidence.observe({
      source: "guest-proof",
      caseKey: selected.key,
      identitySha256: h,
      state: "succeeded",
      contentSha256: h,
    });
    await evidence.result({
      key: selected.key,
      status: "PASS",
      assertions: selected.assertions.map((assertion) => ({
        assertion,
        observationHashes: [digest],
      })),
    });
  }
}
test("full evidence needs successful ordered infrastructure before acceptance", async () => {
  const ctx = ContextSchema.parse(validContext());
  ctx.evidenceFile = join(root, "full-infrastructure.jsonl");
  const evidence = await Evidence.create(ctx);
  try {
    await recordCases(evidence);
    await expect(evidence.finish()).rejects.toThrow(
      "infrastructure_evidence_required",
    );
    for (const rows of [
      [],
      infrastructureRows().reverse(),
      infrastructureRows().slice(0, 1),
      infrastructureRows().map((v) => ({ ...v, unauthenticatedStatus: 200 })),
      infrastructureRows().map((v) => ({ ...v, execSha256: "A".repeat(64) })),
      infrastructureRows().map((v) => ({ ...v, physicalAcceptance: true })),
    ])
      await expect(evidence.infrastructure(rows)).rejects.toThrow();
    await evidence.infrastructure(infrastructureRows());
    await expect(evidence.infrastructure(infrastructureRows())).rejects.toThrow(
      "infrastructure_evidence_duplicate",
    );
    await recordPrerequisites(evidence);
    await evidence.finish();
  } finally {
    await evidence.close();
  }
  const records = (await readFile(ctx.evidenceFile, "utf8"))
    .trim()
    .split("\n")
    .map((line) => JSON.parse(line));
  expect(records.at(-1)).toMatchObject({
    kind: "complete",
    physicalAcceptance: true,
    selected: 24,
    passed: 24,
  });
  expect(records.find((v) => v.kind === "infrastructure").rows).toEqual(
    infrastructureRows(),
  );
});
for (const stage of [
  "infrastructure",
  "filtered",
  "explicit-all-keys",
] as const) {
  test(`${stage} evidence cannot claim full acceptance`, async () => {
    const ctx = ContextSchema.parse(validContext());
    ctx.evidenceFile = join(root, `${stage}.jsonl`);
    const matrix =
      stage === "explicit-all-keys"
        ? REQUIRED_CASES.map((v) => v.key).join(",")
        : REQUIRED_CASES[0]!.key;
    const selection =
      stage === "infrastructure"
        ? { stage: "infrastructure" as const }
        : { stage: "matrix" as const, matrix };
    const cases = stage === "infrastructure" ? [] : selectMatrix(matrix);
    const evidence = await Evidence.create(ctx, selection);
    try {
      if (stage === "infrastructure") {
        await expect(evidence.finish()).rejects.toThrow(
          "infrastructure_evidence_required",
        );
        await evidence.infrastructure(infrastructureRows());
      } else {
        await expect(evidence.finish()).rejects.toThrow(
          "selected_cases_incomplete",
        );
        await recordCases(evidence, cases);
        await evidence.infrastructure(infrastructureRows());
      }
      await recordPrerequisites(evidence);
      await evidence.finish();
    } finally {
      await evidence.close();
    }
    const records = (await readFile(ctx.evidenceFile, "utf8"))
      .trim()
      .split("\n")
      .map((line) => JSON.parse(line));
    expect(records[0].selected).toBe(cases.length);
    expect(records.at(-1)).toMatchObject({
      kind: "stage-complete",
      stageCompleted: true,
      matrixCompleted: false,
      physicalAcceptance: false,
      selected: cases.length,
      passed: cases.length,
    });
    expect(records.some((v) => v.kind === "complete")).toBe(false);
    expect(() => verifyResults([])).toThrow("missing_or_duplicate_case");
  });
}

test("observer accepts the production automation task id and retains ownership guards", async () => {
  const { DatabaseObserver } = await import("./observer.ts");
  const { automationRunId } = await import("../../src/automations/ids.ts");
  const ctx = ContextSchema.parse(validContext());
  ctx.runRoot = root;
  ctx.observer.coordinatorDsnFile = join(root, "observer-own-c.dsn");
  ctx.observer.orchestratorDsnFile = join(root, "observer-own-o.dsn");
  for (const path of [
    ctx.observer.coordinatorDsnFile,
    ctx.observer.orchestratorDsnFile,
  ])
    await writeFile(
      path,
      "postgres://fixture:neutral@database.fixture.example/mcp_unit",
      {
        mode: 0o600,
      },
    );
  // The pools are lazy. Registering ownership does not send a database query.
  const database = await DatabaseObserver.connect(ctx);
  const taskId = `automation:${automationRunId(
    "11111111-1111-4111-8111-111111111111",
    "manual:22222222-2222-4222-8222-222222222222",
  )}`;
  const sessionId = "33333333-3333-4333-8333-333333333333";
  try {
    expect(() => database.own(taskId, sessionId)).not.toThrow();
    expect(() => database.own(taskId, sessionId)).toThrow(
      "observer_owned_session",
    );
    expect(() => database.own("different-task", sessionId)).toThrow(
      "observer_owned_session",
    );
    expect(() => database.own(taskId, "not-a-session")).toThrow(
      "observer_owned_session",
    );
    for (const invalid of [
      "",
      "task space",
      "task\nname",
      "task/name",
      "task\u0000name",
      "a".repeat(129),
    ])
      expect(() =>
        database.own(invalid, "44444444-4444-4444-8444-444444444444"),
      ).toThrow("observer_owned_session");
  } finally {
    await database.close();
  }
});

test("selected dispatcher collects cleaned case failures and stops on unsafe failures", async () => {
  const { runSelectedCases } = await import("./runner.ts");
  const { DatabaseObserver } = await import("./observer.ts");
  const { SCENARIOS } = await import("./manifest.ts");
  const ctx = ContextSchema.parse(validContext());
  ctx.runRoot = root;
  ctx.observer.coordinatorDsnFile = join(root, "selection-c.dsn");
  ctx.observer.orchestratorDsnFile = join(root, "selection-o.dsn");
  for (const path of [
    ctx.observer.coordinatorDsnFile,
    ctx.observer.orchestratorDsnFile,
  ])
    await writeFile(
      path,
      "postgres://fixture:neutral@database.fixture.example/mcp_unit",
      {
        mode: 0o600,
      },
    );
  ctx.endpoints.app.ca.path = join(root, "selection-ca.pem");
  await writeFile(ctx.endpoints.app.ca.path, "unused-neutral-ca", {
    mode: 0o600,
  });
  // Pools and public clients are lazy. No query or request is made by these test drivers.
  const database = await DatabaseObserver.connect(ctx);
  const client = await publicClient(ctx, {
    kind: "cookie",
    value: "neutral=fixture",
  });
  const principals = {
    admin: client,
    owner: client,
    other: client,
    unattended: client,
    automation: client,
    actorIds: {
      admin: "1",
      owner: "2",
      other: "3",
      unattended: "4",
      automation: "5",
    },
  };
  const input = { ctx, database, principals };
  const calls: string[] = [];
  let mode = "pass";
  const events: string[] = [];
  const registry: Record<string, unknown> = Object.fromEntries(
    SCENARIOS.map((s) => [
      s.id,
      async ({ harness }: { harness: string }) => {
        calls.push(`${harness}/${s.id}`);
        if (harness === "claude" && mode === "setup")
          throw new RunnerFailure("ASSERTION_FAILED", "setup_failed");
        try {
          if (
            (harness === "claude" && ["body", "body-cleanup"].includes(mode)) ||
            mode === "both-body"
          )
            throw new RunnerFailure(
              "ASSERTION_FAILED",
              `${harness}_case_assertion`,
            );
          if (harness === "claude" && mode === "nested-cleanup")
            await completeCaseCleanup(async () => {
              throw new RunnerFailure(
                "ASSERTION_FAILED",
                "nested_cleanup_failed",
              );
            });
          if (harness === "claude" && mode === "transport")
            throw new RunnerFailure("TRANSPORT_FAILED", "https_request");
          if (harness === "claude" && mode === "unexpected")
            throw new Error("private-error");
          if (harness === "claude" && mode === "unclassified")
            throw new RunnerFailure(
              "ASSERTION_FAILED",
              "oauth_account_recovery_prepare_phase_unavailable_unclassified",
            );
          if (harness === "claude" && mode === "unsafe-seam")
            throw new RunnerFailure("ASSERTION_FAILED", "private error text");
          if (mode === "incomplete") return [];
          return s.assertions.map((assertion) => ({
            assertion,
            observations: [
              {
                source: "guest-proof",
                identitySha256: h,
                state: "succeeded",
                contentSha256: h,
              },
            ],
          }));
        } catch (error) {
          throw caseBodyFailure(error);
        } finally {
          await completeCaseCleanup(async () => {
            events.push(`cleanup:${harness}`);
            if (
              harness === "claude" &&
              ["cleanup", "body-cleanup"].includes(mode)
            )
              throw new RunnerFailure("ASSERTION_FAILED", "cleanup_failed");
          });
        }
      },
    ]),
  );
  validateDrivers(registry);
  const selection = {
    stage: "matrix" as const,
    matrix: "codex/catalog-fidelity,claude/catalog-fidelity",
  };
  ctx.evidenceFile = join(root, "selected-dispatch.jsonl");
  const evidence = await Evidence.create(ctx, selection);
  try {
    await runSelectedCases(registry, selection, input, evidence);
    expect(calls).toEqual([
      "claude/catalog-fidelity",
      "codex/catalog-fidelity",
    ]);
    await evidence.infrastructure(infrastructureRows());
    await recordPrerequisites(evidence);
    await evidence.finish();
  } finally {
    await evidence.close();
  }
  ctx.evidenceFile = join(root, "selected-failure.jsonl");
  const failed = await Evidence.create(ctx, selection);
  mode = "incomplete";
  calls.length = 0;
  events.length = 0;
  try {
    await expect(
      runSelectedCases(registry, selection, input, failed, async () => {
        events.push("ready");
      }),
    ).rejects.toThrow("driver_subassertions_complete");
    expect(calls).toEqual([
      "claude/catalog-fidelity",
      "codex/catalog-fidelity",
    ]);
    expect(events).toEqual(["cleanup:claude", "ready", "cleanup:codex"]);
    await expect(failed.finish()).rejects.toThrow("selected_cases_incomplete");
  } finally {
    await failed.close();
  }
  const records = (await readFile(ctx.evidenceFile, "utf8"))
    .trim()
    .split("\n")
    .map((line) => JSON.parse(line));
  expect(records.at(-1)).toMatchObject({
    kind: "case",
    key: "codex/catalog-fidelity",
    status: "FAIL",
    seam: "driver_subassertions_complete",
  });
  expect(records.filter((record) => record.kind === "case")).toHaveLength(2);
  expect(
    records.some(
      (record) =>
        record.kind === "complete" || record.kind === "stage-complete",
    ),
  ).toBe(false);
  try {
    for (const variant of [
      { mode: "body", seam: "claude_case_assertion", calls: 2, ready: true },
      {
        mode: "both-body",
        seam: "claude_case_assertion",
        calls: 2,
        ready: true,
      },
      { mode: "cleanup", seam: "cleanup_failed", calls: 1, ready: false },
      { mode: "body-cleanup", seam: "cleanup_failed", calls: 1, ready: false },
      {
        mode: "nested-cleanup",
        seam: "nested_cleanup_failed",
        calls: 1,
        ready: false,
      },
      { mode: "setup", seam: "setup_failed", calls: 1, ready: false },
      { mode: "transport", seam: "https_request", calls: 1, ready: false },
      {
        mode: "unclassified",
        seam: "oauth_account_recovery_prepare_phase_unavailable_unclassified",
        calls: 1,
        ready: false,
      },
      {
        mode: "unsafe-seam",
        seam: "unexpected_runner_error",
        calls: 1,
        ready: false,
      },
      {
        mode: "unexpected",
        seam: "unexpected_runner_error",
        calls: 1,
        ready: false,
      },
      {
        mode: "body",
        seam: "infrastructure_unhealthy",
        calls: 1,
        ready: true,
        unhealthy: true,
      },
      {
        mode: "body",
        seam: "runner_total_deadline",
        calls: 1,
        ready: false,
        expired: true,
      },
      {
        mode: "body",
        seam: "runner_total_deadline",
        calls: 1,
        ready: true,
        expiresDuringHealth: true,
      },
    ]) {
      mode = variant.mode;
      calls.length = 0;
      events.length = 0;
      ctx.evidenceFile = join(
        root,
        `selected-${variant.mode}-${variant.seam}.jsonl`,
      );
      // Deadline cases use distinct files without changing the run identity.
      if (variant.expiresDuringHealth) ctx.evidenceFile += ".during-health";
      const output = await Evidence.create(ctx, selection);
      let ready = false;
      try {
        const deadline = variant.expired
          ? performance.now() - 1
          : performance.now() +
            (variant.expiresDuringHealth ? 500 : ctx.deadlines.totalMs);
        await expect(
          runSelectedCases(
            registry,
            selection,
            input,
            output,
            async () => {
              ready = true;
              events.push("ready");
              if (variant.unhealthy)
                throw new RunnerFailure(
                  "ASSERTION_FAILED",
                  "infrastructure_unhealthy",
                );
              if (variant.expiresDuringHealth)
                await new Promise((resolve) => setTimeout(resolve, 510));
            },
            deadline,
          ),
        ).rejects.toThrow(variant.seam);
        expect(calls).toHaveLength(variant.calls);
        expect(ready).toBe(variant.ready);
        if (variant.calls === 2)
          expect(events).toEqual(["cleanup:claude", "ready", "cleanup:codex"]);
        await expect(output.finish()).rejects.toThrow();
      } finally {
        await output.close();
      }
      const rows = (await readFile(ctx.evidenceFile, "utf8"))
        .trim()
        .split("\n")
        .map((line) => JSON.parse(line));
      expect(rows.filter((row) => row.kind === "case")).toHaveLength(
        variant.calls,
      );
      expect(rows.find((row) => row.kind === "case")).toMatchObject({
        key: "claude/catalog-fidelity",
        status: "FAIL",
      });
      if (variant.calls === 2)
        expect(rows.at(-1)).toMatchObject({
          key: "codex/catalog-fidelity",
          status: variant.mode === "both-body" ? "FAIL" : "PASS",
        });
      expect(
        rows.some(
          (row) => row.kind === "complete" || row.kind === "stage-complete",
        ),
      ).toBe(false);
    }
  } finally {
    await database.close();
  }
});

// Run the generated guest shell against a private proc tree, with no live guest.
test("guest process probes identify the staged harness and keep their guards", async () => {
  const dir = await mkdtemp(join(root, "proc-probe-"));
  const proc = join(dir, "proc");
  const shm = join(dir, "shm");
  const state = join(dir, "state");
  await mkdir(join(proc, "sys/kernel/random"), { recursive: true });
  await mkdir(shm);
  await mkdir(state);
  await writeFile(join(proc, "mounts"), `tmpfs ${shm} tmpfs rw 0 0\n`);
  await writeFile(
    join(proc, "sys/kernel/random/boot_id"),
    "11111111-1111-4111-8111-111111111111\n",
  );
  const ctx = ContextSchema.parse(validContext());
  const session = "probe-session";
  await writeFile(
    join(state, "cold-disk-sentinel"),
    sha256(`${ctx.runId}:${session}:cold-sentinel`),
  );
  async function process(pid: number, argv: string[], env: string[]) {
    const path = join(proc, String(pid));
    await mkdir(path, { recursive: true });
    await writeFile(join(path, "cmdline"), argv.join("\0") + "\0");
    await writeFile(join(path, "environ"), env.join("\0") + "\0");
  }
  const harness = "/opt/fixture/harness";
  await process(
    1,
    [harness, "--connect", "local", "--session-id", session],
    ["ENGRAM_TOOLS=manifest"],
  );
  await process(2, [ctx.guest.cliPath], []);
  await process(3, [harness, "mcp-bridge"], ["ENGRAM_TOOLS=manifest"]);
  await process(4, [harness, "hook-bridge"], ["ENGRAM_TOOLS=manifest"]);
  async function generated(cold: boolean) {
    let argv: string[] = [];
    const client = {
      sessions: {
        async *exec(request: { argv?: string[] }) {
          argv = request.argv ?? [];
          yield create(ExecOutputSchema, {
            event: { case: "started", value: {} },
          });
          yield create(ExecOutputSchema, {
            event: { case: "exit", value: { exitStatus: 1 } },
          });
        },
      },
    };
    try {
      if (cold) await guestColdObservation(ctx, client, session, false);
      else await probeOAuthCredentialAbsence(ctx, client, session);
    } catch {
      /* The stub only captures the command. */
    }
    expect(argv.length).toBeGreaterThan(3);
    const script = argv[2]!
      .replaceAll("/proc/", proc + "/")
      .replaceAll("/dev/shm", shm)
      .replaceAll("/tmp/mcp-stack-fixture", state);
    return () =>
      spawnSync(argv[0]!, [argv[1]!, script, ...argv.slice(3)], {
        encoding: "utf8",
      });
  }
  const oauth = await generated(false);
  const cold = await generated(true);
  expect(oauth().status).toBe(0);
  expect(cold().status).toBe(0);
  await process(
    1,
    [harness, "--vsock-host", "123", "--session-id", session],
    ["ENGRAM_TOOLS=manifest"],
  );
  expect(cold().status).toBe(0);
  await process(5, [harness, "--connect", "local"], ["ENGRAM_TOOLS=manifest"]);
  expect(cold().status).toBe(12);
  await rm(join(proc, "5"), { recursive: true });
  await process(
    3,
    [harness, "mcp-bridge"],
    ["ENGRAM_TOOLS=manifest", "TOKEN=fixture-access-123"],
  );
  expect(oauth().status).toBe(15);
  await process(3, [harness, "mcp-bridge"], []);
  expect(oauth().status).toBe(16);
  await process(3, [harness, "mcp-bridge"], ["ENGRAM_TOOLS=manifest"]);
  await rm(join(proc, "2"), { recursive: true });
  expect(oauth().status).toBe(16);
});

test("managed proxy probe uses the fixture IP and SNI without guest DNS", async () => {
  const dir = await mkdtemp(join(root, "managed-probe-"));
  const bin = join(dir, "bin");
  await mkdir(bin);
  const ctx = ContextSchema.parse(validContext());
  const fixture = ctx.protocolFixtures.modern;
  fixture.runRoot = dir;
  fixture.resourceOrigin = "https://resource.fixture.example:8443";
  const config = JSON.stringify({
    listen: { host: "10.82.7.30", port: 8443 },
    advertised: { resource: fixture.resourceOrigin },
  });
  const configPath = join(dir, "config.json");
  await writeFile(configPath, config, { mode: 0o600 });
  fixture.configuration = { path: configPath, sha256: sha256(config) };
  const managed = new URL(fixture.resourceOrigin).hostname;
  const calls: string[][] = [];
  let tlsStatus = 1;
  async function executable(name: string, body: string) {
    const path = join(bin, name);
    await writeFile(path, `#!/bin/sh\n${body}\n`, { mode: 0o700 });
    await chmod(path, 0o700);
  }
  await executable("timeout", 'shift; exec "$@"');
  await executable("getent", "exit 99");
  const client = {
    sessions: {
      async *exec(request: { argv?: string[] }) {
        const argv = request.argv ?? [];
        calls.push(argv);
        await executable("openssl", `exit ${tlsStatus}`);
        const i = argv.indexOf("-c");
        const result = spawnSync(
          "/bin/sh",
          ["-c", argv[i + 1]!, ...argv.slice(i + 2)],
          { env: { PATH: `${bin}:/usr/bin:/bin` }, encoding: "utf8" },
        );
        yield create(ExecOutputSchema, {
          event: { case: "started", value: {} },
        });
        yield create(ExecOutputSchema, {
          event: { case: "stdout", value: Buffer.from(result.stdout) },
        });
        yield create(ExecOutputSchema, {
          event: { case: "exit", value: { exitStatus: result.status ?? 255 } },
        });
      },
    },
  };
  const result = await probeManagedAuthority(ctx, client, "session");
  expect(result.sni).toBe(managed);
  expect(calls).toHaveLength(1);
  expect(calls[0]).toContain("10.82.7.30:443");
  expect(calls[0]).not.toContain("10.82.7.30:8443");
  expect(calls[0]).toContain(managed);
  expect(calls[0]!.join(" ")).not.toContain("getent");
  fixture.configuration.sha256 = "0".repeat(64);
  await expect(probeManagedAuthority(ctx, client, "session")).rejects.toThrow(
    "managed_fixture_config_digest",
  );
  fixture.configuration.sha256 = sha256(config);
  const wrongOrigin = JSON.stringify({
    listen: { host: "10.82.7.30", port: 8443 },
    advertised: { resource: "https://other.fixture.example:8443" },
  });
  await writeFile(configPath, wrongOrigin);
  fixture.configuration.sha256 = sha256(wrongOrigin);
  await expect(probeManagedAuthority(ctx, client, "session")).rejects.toThrow(
    "managed_fixture_config_binding",
  );
  await writeFile(configPath, config);
  fixture.configuration.sha256 = sha256(config);
  for (const status of [0, 124, 126, 127, 137]) {
    tlsStatus = status;
    await expect(
      probeManagedAuthority(ctx, client, "session"),
    ).rejects.toThrow();
  }
});

test("FC storage checkpoint must pass before infrastructure evidence permits cases", async () => {
  const { completeInfrastructure } = await import("./runner.ts");
  for (const failure of [false, true]) {
    const order: string[] = [];
    const checkpoint = async () => {
      order.push("physical");
      if (failure) throw Error("physical storage failure");
    };
    const evidence = {
      infrastructure: async () => {
        order.push("evidence");
      },
    };
    const task = completeInfrastructure(
      "firecracker",
      infrastructureRows(),
      evidence,
      { infrastructureReady: checkpoint },
    );
    if (failure) {
      await expect(task).rejects.toThrow("physical storage failure");
      expect(order).toEqual(["physical"]);
    } else {
      await task;
      expect(order).toEqual(["physical", "evidence"]);
    }
  }
  let called = false;
  await completeInfrastructure(
    "vz",
    infrastructureRows(),
    { infrastructure: async () => {} },
    {
      infrastructureReady: async () => {
        called = true;
      },
    },
  );
  expect(called).toBe(false);
});
