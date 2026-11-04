import { afterAll, beforeAll, expect, test } from "bun:test";
import { chmod, mkdtemp, writeFile, symlink, rm, readFile } from "node:fs/promises";
import { join } from "node:path";
import { createServer } from "node:https";
import { X509Certificate } from "node:crypto";
import {
  ContextSchema,
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
import { Evidence } from "./evidence.ts";
import { requirePrerequisites } from "./preflight.ts";
import { safeFailure, validateDrivers } from "./runner.ts";
import { drivers } from "./run.ts";
import { isProofFilename, readGuestFile } from "./guest.ts";
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
  expect(() => selectMatrix("claude")).toThrow("matrix_all_required");
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
  ).toEqual({ cookie: "session=neutral" });
  expect(credentialHeaders({ kind: "api-key", value: "engk_neutral" })).toEqual(
    { "x-api-key": "engk_neutral" },
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
  const { ProfileSchema, UpdateProfileRequestSchema } = await import(
    "../../src/gen/engram/app/v1/profile_pb.ts"
  );
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
  const requests: { url: string; cookie?: string; apiKey?: string; origin?: string }[] = [];
  let status = 200;
  let body = bytes;
  let size = String(body.length);
  let hash = sha256(body);
  let stall = false;
  let chunked = false;
  const server = createServer({ key: await readFile(keyPath), cert }, (req, res) => {
    requests.push({
      url: req.url ?? "",
      cookie: req.headers.cookie,
      apiKey: req.headers["x-api-key"]?.toString(),
      origin: req.headers.origin,
    });
    if (stall) return;
    res.writeHead(status, {
      "content-type": "application/octet-stream",
      ...(chunked ? {} : { "content-length": size }),
      "x-content-sha256": hash,
      ...(status === 302 ? { location: "/redirect-target" } : {}),
    });
    res.end(body);
  });
  await new Promise<void>((resolve) => server.listen(0, "localhost", resolve));
  try {
    const address = server.address();
    if (!address || typeof address === "string") throw new Error("server_address");
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
      expect(await readGuestFile(ctx, client, "owned-session", path)).toEqual(bytes);
      expect(requests.at(-1)).toEqual({
        url: "/api/v1/sessions/owned-session/files?path=%2Ftmp%2Fmcp-stack-fixture%2Fproof.json",
        cookie: credential.kind === "cookie" ? credential.value : undefined,
        apiKey: credential.kind === "api-key" ? credential.value : undefined,
        origin: ctx.endpoints.app.origin,
      });
    }
    const client = await publicClient(ctx, { kind: "cookie", value: "fixture_session=owner" });
    for (const invalid of [
      "../proof",
      path + "/child",
      ctx.guest.stateDirectory + "/..",
      path + "?x=1",
      path + ";id",
      path + "\n",
    ]) {
      const before = requests.length;
      await expect(readGuestFile(ctx, client, "owned-session", invalid)).rejects.toThrow(
        "guest_proof_path",
      );
      expect(requests.length).toBe(before);
    }
    for (const denied of [401, 403, 404, 302]) {
      status = denied;
      const before = requests.length;
      await expect(readGuestFile(ctx, client, "owned-session", path)).rejects.toThrow(
        "guest_file_status",
      );
      expect(requests.length).toBe(before + 1);
    }
    status = 200;
    const pin = ctx.endpoints.app.serverFingerprint;
    ctx.endpoints.app.serverFingerprint = Array(32).fill("00").join(":");
    const beforeBadPin = requests.length;
    await expect(readGuestFile(ctx, client, "owned-session", path)).rejects.toThrow(
      "guest_file_download",
    );
    expect(requests.length).toBe(beforeBadPin);
    ctx.endpoints.app.serverFingerprint = pin;
    size = String(bytes.length + 1);
    await expect(readGuestFile(ctx, client, "owned-session", path)).rejects.toThrow(
      "guest_file_download",
    );
    size = String(bytes.length);
    hash = "invalid";
    await expect(readGuestFile(ctx, client, "owned-session", path)).rejects.toThrow(
      "guest_file_metadata",
    );
    hash = "0".repeat(64);
    await expect(readGuestFile(ctx, client, "owned-session", path)).rejects.toThrow(
      "guest_file_integrity",
    );
    hash = sha256(bytes);
    size = String(1024 * 1024 + 1);
    await expect(readGuestFile(ctx, client, "owned-session", path)).rejects.toThrow(
      "guest_file_metadata",
    );
    chunked = true;
    await expect(readGuestFile(ctx, client, "owned-session", path)).rejects.toThrow(
      "guest_file_metadata",
    );
    chunked = false;
    body = Buffer.alloc(1024 * 1024, 65);
    hash = sha256(body);
    size = String(body.length);
    expect(await readGuestFile(ctx, client, "owned-session", path)).toEqual(body);
    body = Buffer.alloc(0);
    hash = sha256(body);
    size = "0";
    expect(await readGuestFile(ctx, client, "owned-session", path)).toEqual(body);
    ctx.deadlines.requestMs = 50;
    stall = true;
    await expect(readGuestFile(ctx, client, "owned-session", path)).rejects.toThrow(
      "guest_file_download",
    );
    expect(requests.every((request) => request.url.startsWith("/api/v1/sessions/"))).toBe(true);
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
  const server = createServer({ key: await readFile(keyPath), cert }, (req, res) => {
    if (req.url === "/rpc/engram.app.v1.IntegrationService/DiscoverMcp") {
      res.writeHead(400, { "content-type": "application/json", "x-private": secret });
      res.end(JSON.stringify(errorBody));
    } else if (req.url === "/rpc/engram.app.v1.SessionService/Exec") {
      res.writeHead(200, { "content-type": "application/connect+proto" });
      res.write(envelope(0, toBinary(ExecOutputSchema, first)));
      res.end(
        envelope(
          2,
          Buffer.from(
            JSON.stringify(
              streamFailure ? { error: errorBody, metadata: { "x-private": [secret] } } : {},
            ),
          ),
        ),
      );
    } else {
      res.writeHead(404);
      res.end();
    }
  });
  await new Promise<void>((resolve) => server.listen(0, "localhost", resolve));
  try {
    const address = server.address();
    if (!address || typeof address === "string") throw new Error("server_address");
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
    if (!(original instanceof ConnectError)) throw new Error("expected_connect_error");
    expect(original.code).toBe(Code.FailedPrecondition);
    expect(original.rawMessage).toBe(secret);
    expect(original.metadata.get("x-private")).toBe(secret);
    expect(await expectRejected(() => Promise.reject(original), [Code.FailedPrecondition])).toBe(
      Code.FailedPrecondition,
    );
    const failure = safeFailure(original);
    expect(failure.seam).toBe("public_rpc_IntegrationService.DiscoverMcp_code_9");
    expect(safeFailure(original).seam).toBe(failure.seam);
    expect(original.rawMessage).toBe(secret);
    expect(Object.keys(original).sort()).toEqual(
      Object.keys(new ConnectError(secret, Code.FailedPrecondition)).sort(),
    );
    expect(JSON.stringify(failure)).not.toContain("private");
    expect(safeFailure(new ConnectError(secret, Code.FailedPrecondition)).seam).toBe(
      "public_rpc_code_9",
    );
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
    if (!(deferred instanceof ConnectError)) throw new Error("expected_stream_error");
    expect(deferred.rawMessage).toBe(secret);
    expect(deferred.metadata.get("x-private")).toBe(secret);
    expect(safeFailure(deferred).seam).toBe("public_rpc_SessionService.Exec_code_9");
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
  ctx.protocolFixtures.modern.resourceKeyFile = join(root, "catalog-resource.key");
  await writeFile(ctx.protocolFixtures.modern.resourceKeyFile, "fixture-neutral-resource-key", {
    mode: 0o600,
  });
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
            req.tools.map((tool) => [tool.upstreamName, tool.classification, tool.enabled]),
          ).toEqual([
            ["read_counter", "read", true],
            ["write_counter", "write", true],
          ]);
          return { catalog: published };
        },
        setConnectionEnabled(req, rpc) {
          calls.push("enable");
          expect(calls).toEqual(["create", "authorize", "discover", "review", "publish", "enable"]);
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
    if (!address || typeof address === "string") throw new Error("server_address");
    ctx.endpoints.app = {
      origin: `https://localhost:${address.port}`,
      ca: { path: certPath, sha256: sha256(cert) },
      serverFingerprint: new X509Certificate(cert).fingerprint256,
    };
    const client = await publicClient(ctx, { kind: "cookie", value: "fixture_session=admin" });
    const result = await createCatalog(ctx, client, "catalog");
    expect(calls).toEqual(["create", "authorize", "discover", "review", "publish", "enable"]);
    expect(result.connection).toEqual(enabled);
    expect(result.catalog).toEqual(published);
    for (mode of ["rpc-failure", "wrong-id", "wrong-revision", "disabled", "missing"]) {
      calls.length = 0;
      await expect(createCatalog(ctx, client, "catalog")).rejects.toThrow(
        mode === "rpc-failure" ? "enable refused" : "enabled_connection",
      );
      expect(calls).toEqual(["create", "authorize", "discover", "review", "publish", "enable"]);
    }
  } finally {
    server.closeAllConnections();
    await new Promise<void>((resolve, reject) =>
      server.close((error) => (error ? reject(error) : resolve())),
    );
  }
});
