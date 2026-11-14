import { open, type FileHandle } from "node:fs/promises";
import { isDeepStrictEqual } from "node:util";
import { z } from "zod";
import { requireThat, sha256, type Context } from "./context.ts";
import {
  FULL_MATRIX,
  REQUIRED_CASES,
  selectedCases,
  verifyResults,
  type CaseResult,
  type RunSelection,
} from "./manifest.ts";

const observation = z.strictObject({
  source: z.enum([
    "public-rpc",
    "fixture-observer",
    "coordinator-read-only",
    "orchestrator-read-only",
    "host-read-only",
    "guest-proof",
    "supervisor",
  ]),
  caseKey: z.enum(REQUIRED_CASES.map((v) => v.key)),
  // Object identities are hashed before reaching this sink. Never store arguments, results or headers.
  identitySha256: z.string().regex(/^[a-f0-9]{64}$/),
  actorSha256: z
    .string()
    .regex(/^[a-f0-9]{64}$/)
    .optional(),
  state: z.enum([
    "created",
    "started",
    "current",
    "denied",
    "waiting_approval",
    "authorized",
    "dispatching",
    "succeeded",
    "failed",
    "canceled",
    "outcome_unknown",
    "relayed",
    "acknowledged",
    "target_gone",
    "retired",
    "stopped",
    "running",
  ]),
  contentSha256: z.string().regex(/^[a-f0-9]{64}$/),
  count: z.number().int().nonnegative().safe().optional(),
  bytes: z
    .number()
    .int()
    .nonnegative()
    .max(4 * 1024 * 1024)
    .optional(),
});
const infrastructureRow = z.strictObject({
  mode: z.enum(["ordinary", "managed"]),
  execSha256: z.string().regex(/^[a-f0-9]{64}$/),
  uploadSha256: z.string().regex(/^[a-f0-9]{64}$/),
  downloadSha256: z.string().regex(/^[a-f0-9]{64}$/),
  sizeBytes: z
    .number()
    .int()
    .positive()
    .safe()
    .max(4 * 1024 * 1024),
  unauthenticatedStatus: z.literal(401),
  nonOwnerStatus: z.literal(404),
});
export type InfrastructureRow = z.infer<typeof infrastructureRow>;
const infrastructure = z.tuple([
  infrastructureRow.extend({ mode: z.literal("ordinary") }),
  infrastructureRow.extend({ mode: z.literal("managed") }),
]);
export type Observation = z.infer<typeof observation>;
const hash = z.string().regex(/^[a-f0-9]{64}$/);
const prerequisites = z.strictObject({
  supervisorSha256: hash,
  publicationSha256: hash,
  publicStateSha256: hash,
});
const caseResult = z.strictObject({
  key: z.string(),
  status: z.enum(["PASS", "FAIL", "NEEDS_CONTEXT"]),
  assertions: z.array(
    z.strictObject({
      assertion: z.string(),
      observationHashes: z.array(hash),
    }),
  ),
  seam: z
    .string()
    .regex(/^[a-zA-Z0-9_./:-]{1,200}$/)
    .optional(),
});
const runSelection = z.discriminatedUnion("stage", [
  z.strictObject({ stage: z.literal("infrastructure") }),
  z.strictObject({ stage: z.literal("matrix"), matrix: z.string() }),
]);

/** Writer and reader use the same assertion, provenance and completion checks. */
class EvidenceState {
  private sequence = 0;
  private readonly observed = new Map<string, Set<string>>();
  private readonly results: CaseResult[] = [];
  private infrastructureVerified = false;
  private prerequisitesRecorded = false;
  private completed = false;
  private readonly selected;
  readonly selection: RunSelection;
  constructor(selection: RunSelection) {
    this.selection = runSelection.parse(selection);
    this.selected = selectedCases(this.selection);
  }
  private active() {
    requireThat(!this.completed, "evidence_already_complete");
  }
  header(ctx: Context) {
    return {
      version: 1,
      kind: "run",
      runId: ctx.runId,
      backend: ctx.backend,
      sourceCommit: ctx.sourceCommit,
      selected: this.selected.length,
      ...this.selection,
      physicalAcceptance: false,
      artifactSha256: Object.fromEntries(
        Object.entries(ctx.artifacts).map(([key, value]) => [
          key,
          value.sha256,
        ]),
      ),
      publicationIdentity: ctx.publication.candidateIdentity,
    };
  }
  observe(input: unknown) {
    this.active();
    const value = observation.parse(input);
    requireThat(
      this.selected.some((c) => c.key === value.caseKey),
      "evidence_case_identity",
    );
    const digest = sha256(JSON.stringify(value));
    const hashes = this.observed.get(value.caseKey) ?? new Set<string>();
    hashes.add(digest);
    this.observed.set(value.caseKey, hashes);
    return {
      version: 1,
      kind: "observation",
      sequence: ++this.sequence,
      ...value,
      sha256: digest,
    };
  }
  prerequisites(input: unknown) {
    this.active();
    const value = prerequisites.parse(input);
    requireThat(
      !this.prerequisitesRecorded,
      "prerequisites_evidence_duplicate",
    );
    this.prerequisitesRecorded = true;
    return { version: 1, kind: "prerequisites", ...value };
  }
  infrastructure(input: unknown) {
    this.active();
    const rows = infrastructure.parse(input);
    requireThat(
      !this.infrastructureVerified,
      "infrastructure_evidence_duplicate",
    );
    requireThat(
      rows.every((row) => row.uploadSha256 === row.downloadSha256),
      "infrastructure_file_digest",
    );
    this.infrastructureVerified = true;
    return { version: 1, kind: "infrastructure", rows };
  }
  result(input: unknown) {
    this.active();
    const result = caseResult.parse(input);
    const expected = this.selected.find((c) => c.key === result.key);
    requireThat(
      expected && !this.results.some((r) => r.key === result.key),
      "evidence_case_identity",
    );
    requireThat(
      result.assertions.every(
        (a) =>
          expected.assertions.some((name) => name === a.assertion) &&
          a.observationHashes.length > 0 &&
          a.observationHashes.every((h) =>
            this.observed.get(result.key)?.has(h),
          ),
      ),
      "case_evidence_provenance",
    );
    if (result.status === "PASS")
      requireThat(
        result.assertions.length === expected.assertions.length &&
          new Set(result.assertions.map((a) => a.assertion)).size ===
            expected.assertions.length,
        "case_subassertion_count",
      );
    this.results.push(result);
    return { version: 1, kind: "case", ...result };
  }
  finish() {
    this.active();
    const full =
      this.selection.stage === "matrix" && this.selection.matrix === "all";
    if (full) verifyResults(this.results);
    else
      requireThat(
        this.results.length === this.selected.length &&
          this.selected.every((c) =>
            this.results.some((r) => r.key === c.key && r.status === "PASS"),
          ),
        "selected_cases_incomplete",
      );
    requireThat(
      this.infrastructureVerified,
      "infrastructure_evidence_required",
    );
    requireThat(this.prerequisitesRecorded, "prerequisites_evidence_required");
    this.completed = true;
    return {
      version: 1,
      kind: full ? "complete" : "stage-complete",
      ...this.selection,
      selected: this.selected.length,
      passed: this.results.length,
      stageCompleted: !full,
      matrixCompleted: full,
      physicalAcceptance: full,
    };
  }
}

export const EVIDENCE_MAX_BYTES = 16 * 1024 * 1024;

/** Replay the bounded stream through the writer's validators before adapter success. */
export function verifyEvidence(
  ctx: Context,
  selection: RunSelection,
  source: string,
) {
  requireThat(
    Buffer.byteLength(source) <= EVIDENCE_MAX_BYTES && source.endsWith("\n"),
    "evidence_stream_bound",
  );
  const lines = source.split("\n");
  lines.pop();
  requireThat(lines.length >= 2, "evidence_stream_records");
  const state = new EvidenceState(selection);
  requireThat(
    isDeepStrictEqual(JSON.parse(lines[0]!), state.header(ctx)),
    "evidence_run_header",
  );
  let completed = false;
  for (let index = 1; index < lines.length; index++) {
    const record = z
      .looseObject({ version: z.literal(1), kind: z.string() })
      .parse(JSON.parse(lines[index]!));
    const { version, kind, ...body } = record;
    switch (kind) {
      case "prerequisites":
        requireThat(
          isDeepStrictEqual(record, state.prerequisites(body)),
          "evidence_prerequisites_record",
        );
        break;
      case "infrastructure": {
        const value = z.strictObject({ rows: infrastructure }).parse(body);
        requireThat(
          isDeepStrictEqual(record, state.infrastructure(value.rows)),
          "evidence_infrastructure_record",
        );
        break;
      }
      case "observation": {
        const { sequence, sha256: digest, ...value } = body;
        const expected = state.observe(value);
        requireThat(
          sequence === expected.sequence && digest === expected.sha256,
          "evidence_observation_digest",
        );
        break;
      }
      case "case":
        state.result(body);
        break;
      case "complete":
      case "stage-complete":
        requireThat(index === lines.length - 1, "evidence_terminal_position");
        requireThat(
          isDeepStrictEqual(record, state.finish()),
          "evidence_completion_record",
        );
        completed = true;
        break;
      default:
        requireThat(false, "evidence_record_kind");
    }
  }
  requireThat(completed, "evidence_completion_missing");
}

export class Evidence {
  private constructor(
    private readonly handle: FileHandle,
    private readonly state: EvidenceState,
  ) {}
  static async create(ctx: Context, selection: RunSelection = FULL_MATRIX) {
    const state = new EvidenceState(selection);
    const handle = await open(ctx.evidenceFile, "wx", 0o600);
    const evidence = new Evidence(handle, state);
    try {
      await evidence.line(state.header(ctx));
    } catch (error) {
      await handle.close();
      throw error;
    }
    return evidence;
  }
  private async line(value: unknown) {
    await this.handle.writeFile(JSON.stringify(value) + "\n");
  }
  async observe(input: Observation) {
    const value = this.state.observe(input);
    await this.line(value);
    return value.sha256;
  }
  async prerequisites(input: z.infer<typeof prerequisites>) {
    await this.line(this.state.prerequisites(input));
  }
  async infrastructure(input: unknown) {
    await this.line(this.state.infrastructure(input));
  }
  async result(result: CaseResult) {
    await this.line(this.state.result(result));
  }
  async finish() {
    await this.line(this.state.finish());
    await this.handle.sync();
  }
  async close() {
    await this.handle.sync();
    await this.handle.close();
  }
}
