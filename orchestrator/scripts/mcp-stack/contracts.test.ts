import { afterAll, beforeAll, expect, test } from "bun:test";
import { mkdtemp, writeFile, symlink, rm } from "node:fs/promises";
import { join } from "node:path";
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
import { credentialHeaders } from "./transport.ts";
import { modelToolName } from "./bootstrap.ts";
import { StatusSchema } from "./supervisor.ts";
import { Evidence } from "./evidence.ts";
import { parseArgs } from "./cli.ts";
import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import {
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
