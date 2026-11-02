import { open, type FileHandle } from "node:fs/promises";
import { z } from "zod";
import { requireThat, sha256, type Context } from "./context.ts";
import { REQUIRED_CASES, verifyResults, type CaseResult } from "./manifest.ts";

const observation = z.strictObject({
  source: z.enum([
    "public-rpc",
    "fixture-observer",
    "coordinator-read-only",
    "orchestrator-read-only",
    "guest-proof",
    "supervisor",
  ]),
  caseKey: z.enum(REQUIRED_CASES.map((v) => v.key)),
  // Object identities are hashed before reaching this sink. Never store arguments, results or headers.
  identitySha256: z.string().regex(/^[a-f0-9]{64}$/),
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
export type Observation = z.infer<typeof observation>;
export class Evidence {
  private sequence = 0;
  private readonly observed = new Map<string, Set<string>>();
  private readonly results: CaseResult[] = [];
  private constructor(private readonly handle: FileHandle) {}
  static async create(ctx: Context) {
    const handle = await open(ctx.evidenceFile, "wx", 0o600);
    const evidence = new Evidence(handle);
    await evidence.line({
      version: 1,
      kind: "run",
      runId: ctx.runId,
      backend: ctx.backend,
      sourceCommit: ctx.sourceCommit,
      selected: REQUIRED_CASES.length,
      physicalAcceptance: false,
      artifactSha256: Object.fromEntries(
        Object.entries(ctx.artifacts).map(([key, value]) => [
          key,
          value.sha256,
        ]),
      ),
      publicationIdentity: ctx.publication.candidateIdentity,
    });
    return evidence;
  }
  private async line(value: unknown) {
    await this.handle.writeFile(JSON.stringify(value) + "\n");
  }
  async observe(input: Observation) {
    const value = observation.parse(input);
    const digest = sha256(JSON.stringify(value));
    const hashes = this.observed.get(value.caseKey) ?? new Set<string>();
    hashes.add(digest);
    this.observed.set(value.caseKey, hashes);
    await this.line({
      version: 1,
      kind: "observation",
      sequence: ++this.sequence,
      ...value,
      sha256: digest,
    });
    return digest;
  }
  async result(result: CaseResult) {
    const expected = REQUIRED_CASES.find((c) => c.key === result.key);
    requireThat(
      expected && !this.results.some((r) => r.key === result.key),
      "evidence_case_identity",
    );
    requireThat(
      !result.seam || /^[a-zA-Z0-9_./:-]{1,200}$/.test(result.seam),
      "sanitized_seam",
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
    await this.line({ version: 1, kind: "case", ...result });
  }
  async finish() {
    verifyResults(this.results);
    await this.line({
      version: 1,
      kind: "complete",
      selected: REQUIRED_CASES.length,
      passed: this.results.length,
      physicalAcceptance: true,
    });
    await this.handle.sync();
  }
  async close() {
    await this.handle.sync();
    await this.handle.close();
  }
}
