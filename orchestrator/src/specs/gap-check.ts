import {
  analyzeTraceability,
  applyOutsideIn,
  renderMarkdown,
  schema,
  TraceabilityInputError,
  type GapFinding,
  type GapFindingSeverity,
  type GapProposedDiff,
  type TraceabilityBlock,
  type TraceabilityLayer,
  type TraceabilityMatrix,
  type TraceabilitySection,
} from "@engrams/spec-document";
import { randomUUID } from "node:crypto";
import type { Node as ProseMirrorNode } from "prosemirror-model";
import type { Pool } from "pg";

import type { GapFindingDisposition } from "../db/schema.ts";
import type { SpecRailMetadata, SpecRailStore } from "../routes/spec-rail.ts";
import { proseMirrorDocument, type SpecDocumentService } from "./doc-service.ts";
import { stableQuestionId } from "./tool-service.ts";
import type { SpecMutationContext, SpecMutationResult } from "../tools/specs.ts";

export interface GapCheckFinding extends GapFinding {
  disposition: GapFindingDisposition;
  openQuestionId: string | null;
  disposedBy: string | null;
  disposedAt: Date | null;
}

export interface GapCheckRun {
  id: string;
  specId: string;
  sessionId: string | null;
  /** The document revision this pass covered. */
  semanticDocSeq: bigint;
  /** Set when a fatal finding halted the pass outside-in (R32). */
  stoppedAtLayerKey: string | null;
  suppressedCount: number;
  matrix: TraceabilityMatrix;
  findings: readonly GapCheckFinding[];
  startedBy: string | null;
  createdAt: Date;
}

/**
 * What the publish gate reads to decide whether the gap check is fresh
 * enough for this drafting round.
 */
export interface GapCheckStatus {
  run: GapCheckRun | null;
  currentSemanticDocSeq: bigint;
  /** True when no pass has run, or the document moved on since the last one. */
  stale: boolean;
}

/** A red-team finding: agent judgment the deterministic pass cannot compute. */
export interface RedTeamFinding {
  layerKey: string;
  sectionId: string;
  severity: GapFindingSeverity;
  summary: string;
  detail: string;
}

/** A remedy the agent offers for one computed finding, for a person to accept. */
export interface ProposedDiffInput {
  findingId: string;
  after: string;
}

export interface GapCheckRunInput {
  specId: string;
  sessionId: string | null;
  /** Makes a replayed request return its original run instead of a second one. */
  requestFingerprint: string;
  actorUserId: string | null;
  redTeam?: readonly RedTeamFinding[];
  proposedDiffs?: readonly ProposedDiffInput[];
}

export type DispositionAction = "open_question" | "accept_diff" | "dismiss";

export interface DisposeFindingInput {
  runId: string;
  findingId: string;
  action: DispositionAction;
  actorUserId: string | null;
}

export class GapCheckError extends Error {
  constructor(
    readonly code:
      | "spec_not_found"
      | "run_not_found"
      | "finding_not_found"
      | "already_disposed"
      | "no_proposed_diff"
      | "untraceable_document"
      | "unknown_layer"
      | "unknown_section",
    message: string,
  ) {
    super(message);
    this.name = "GapCheckError";
  }
}

export interface GapCheckStore {
  findRunByFingerprint(specId: string, fingerprint: string): Promise<GapCheckRun | null>;
  latestRun(specId: string): Promise<GapCheckRun | null>;
  readRun(runId: string): Promise<GapCheckRun | null>;
  insertRun(run: GapCheckRun, requestFingerprint: string): Promise<void>;
  /** False when the finding was already disposed of by somebody else. */
  markDisposition(input: {
    runId: string;
    findingId: string;
    disposition: GapFindingDisposition;
    openQuestionId: string | null;
    disposedBy: string | null;
    disposedAt: Date;
  }): Promise<boolean>;
}

/** The two document services the gap check reuses to land a disposition. */
export interface GapCheckDocumentService {
  addOpenQuestion(
    specId: string,
    input: SpecMutationContext & { sectionId: string; question: string },
  ): Promise<SpecMutationResult>;
  updateSection(
    specId: string,
    input: SpecMutationContext & { sectionId: string; markdown: string },
  ): Promise<SpecMutationResult>;
}

export interface GapCheckServiceOptions {
  documents: Pick<SpecDocumentService, "syncFromLog">;
  railStore: SpecRailStore;
  store: GapCheckStore;
  toolDocuments: GapCheckDocumentService;
  now: () => Date;
}

/**
 * Runs the gap check and records it (ADR 0114 D6, R30-R33).
 *
 * The pass itself never edits the document. It writes gap-check rows only.
 * A finding reaches the document one way: a person disposes of it, and that
 * disposition opens a question at the anchor or accepts a proposed diff (R28).
 */
export class GapCheckService {
  constructor(private readonly options: GapCheckServiceOptions) {}

  async status(specId: string): Promise<GapCheckStatus> {
    const loaded = await this.options.documents.syncFromLog(specId);
    const run = await this.options.store.latestRun(specId);
    return {
      run,
      currentSemanticDocSeq: loaded.semanticDocSeq,
      stale: run === null || run.semanticDocSeq < loaded.semanticDocSeq,
    };
  }

  async readRun(runId: string): Promise<GapCheckRun> {
    const run = await this.options.store.readRun(runId);
    if (!run) throw new GapCheckError("run_not_found", `Gap check run ${runId} does not exist.`);
    return run;
  }

  async run(input: GapCheckRunInput): Promise<GapCheckRun> {
    const existing = await this.options.store.findRunByFingerprint(
      input.specId,
      input.requestFingerprint,
    );
    if (existing) return existing;

    const metadata = await this.options.railStore.readMetadata(input.specId);
    if (!metadata) {
      throw new GapCheckError("spec_not_found", `Spec ${input.specId} does not exist.`);
    }
    const loaded = await this.options.documents.syncFromLog(input.specId);
    const document = proseMirrorDocument(loaded.doc);
    const sections = readSections(document, metadata);
    const layers: TraceabilityLayer[] = metadata.layers.map((layer) => ({
      key: layer.key,
      title: layer.title,
    }));

    let analysis;
    try {
      analysis = analyzeTraceability({ layers, sections });
    } catch (error) {
      if (error instanceof TraceabilityInputError) {
        throw new GapCheckError("untraceable_document", error.message);
      }
      throw error;
    }

    const redTeam = (input.redTeam ?? []).map((finding, index) =>
      redTeamFinding(finding, index, layers, sections),
    );
    const withDiffs = attachProposedDiffs(
      [...analysis.findings, ...redTeam],
      input.proposedDiffs ?? [],
      document,
      sections,
    );
    const outsideIn = applyOutsideIn(withDiffs, layers);

    const run: GapCheckRun = {
      id: randomUUID(),
      specId: input.specId,
      sessionId: input.sessionId,
      semanticDocSeq: loaded.semanticDocSeq,
      stoppedAtLayerKey: outsideIn.stoppedAtLayerKey,
      suppressedCount: outsideIn.suppressedCount,
      matrix: analysis.matrix,
      findings: outsideIn.findings.map((finding) => ({
        ...finding,
        disposition: "pending" as const,
        openQuestionId: null,
        disposedBy: null,
        disposedAt: null,
      })),
      startedBy: input.actorUserId,
      createdAt: this.options.now(),
    };
    await this.options.store.insertRun(run, input.requestFingerprint);
    // A concurrent identical request may have won the unique fingerprint, so
    // read back rather than trust the local object.
    const stored = await this.options.store.findRunByFingerprint(
      input.specId,
      input.requestFingerprint,
    );
    return stored ?? run;
  }

  /**
   * Land one finding in the document, the only way a finding ever reaches it.
   * Returns the updated run.
   */
  async disposeFinding(input: DisposeFindingInput): Promise<GapCheckRun> {
    const run = await this.readRun(input.runId);
    const finding = run.findings.find((candidate) => candidate.id === input.findingId);
    if (!finding) {
      throw new GapCheckError(
        "finding_not_found",
        `Gap check run ${input.runId} has no finding ${input.findingId}.`,
      );
    }
    if (finding.disposition !== "pending") {
      throw new GapCheckError(
        "already_disposed",
        `Finding ${input.findingId} was already ${finding.disposition}.`,
      );
    }

    const context: SpecMutationContext = {
      ...(input.actorUserId === null ? {} : { actorUserId: input.actorUserId }),
      sessionId: run.sessionId ?? run.id,
      toolCallId: `gap-finding:${run.id}:${finding.id}`,
    };

    let disposition: GapFindingDisposition;
    let openQuestionId: string | null = null;
    switch (input.action) {
      case "open_question": {
        await this.options.toolDocuments.addOpenQuestion(run.specId, {
          ...context,
          sectionId: finding.sectionId,
          question: finding.detail,
        });
        disposition = "question_opened";
        // The same derivation addOpenQuestion used, so the recorded link
        // points at the row it actually created.
        openQuestionId = stableQuestionId(run.specId, context.sessionId, context.toolCallId);
        break;
      }
      case "accept_diff": {
        if (!finding.proposedDiff) {
          throw new GapCheckError(
            "no_proposed_diff",
            `Finding ${finding.id} carries no proposed diff to accept.`,
          );
        }
        await this.options.toolDocuments.updateSection(run.specId, {
          ...context,
          sectionId: finding.proposedDiff.sectionId,
          markdown: finding.proposedDiff.after,
        });
        disposition = "diff_accepted";
        break;
      }
      case "dismiss":
        disposition = "dismissed";
        break;
    }

    await this.options.store.markDisposition({
      runId: run.id,
      findingId: finding.id,
      disposition,
      openQuestionId,
      disposedBy: input.actorUserId,
      disposedAt: this.options.now(),
    });
    return this.readRun(run.id);
  }
}

/** Adapt the live document into the analyzer's input. */
export function readSections(
  document: ProseMirrorNode,
  metadata: SpecRailMetadata,
): TraceabilitySection[] {
  const rules = new Map(metadata.sections.map((section) => [section.key, section]));
  const sections: TraceabilitySection[] = [];
  document.forEach((node) => {
    if (node.type !== schema.nodes.section) return;
    const id = node.attrs["id"];
    const key = node.attrs["templateSectionKey"];
    if (typeof id !== "string" || typeof key !== "string" || !node.firstChild) {
      throw new Error("Every spec section must keep its stable template identity.");
    }
    const rule = rules.get(key);
    if (!rule) throw new Error(`Spec section ${id} has no template rule.`);
    sections.push({
      id,
      key,
      title: node.firstChild.textContent || id,
      layerKey: rule.layerKey,
      state: metadata.states.get(id)?.state ?? "empty",
      blocks: readBlocks(node),
    });
  });
  return sections;
}

/** The section body, one entry per top-level block. The heading is not a claim. */
function readBlocks(section: ProseMirrorNode): TraceabilityBlock[] {
  const blocks: TraceabilityBlock[] = [];
  section.forEach((child, _offset, index) => {
    if (index === 0) return; // the section heading
    const source = child.attrs["source"];
    // A diagram is an atom, so its claim lives in the source, not in text.
    const text = child.textContent || (typeof source === "string" ? source : "");
    blocks.push({ index: blocks.length, text });
  });
  return blocks;
}

function redTeamFinding(
  finding: RedTeamFinding,
  index: number,
  layers: readonly TraceabilityLayer[],
  sections: readonly TraceabilitySection[],
): GapFinding {
  if (!layers.some((layer) => layer.key === finding.layerKey)) {
    throw new GapCheckError(
      "unknown_layer",
      `A red-team finding names an unknown layer: ${finding.layerKey}`,
    );
  }
  const section = sections.find((candidate) => candidate.id === finding.sectionId);
  if (!section) {
    throw new GapCheckError(
      "unknown_section",
      `A red-team finding names an unknown section: ${finding.sectionId}`,
    );
  }
  return {
    id: `red_team:${finding.sectionId}:${index}`,
    kind: "red_team",
    severity: finding.severity,
    layerKey: finding.layerKey,
    sectionId: section.id,
    sectionTitle: section.title,
    requirementId: null,
    summary: finding.summary,
    detail: finding.detail,
    proposedDiff: null,
  };
}

/**
 * Attach the agent's remedies to computed findings. `before` is read from the
 * document, never taken from the caller, so the diff a person accepts is the
 * one they were shown.
 */
function attachProposedDiffs(
  findings: readonly GapFinding[],
  diffs: readonly ProposedDiffInput[],
  document: ProseMirrorNode,
  sections: readonly TraceabilitySection[],
): GapFinding[] {
  if (diffs.length === 0) return [...findings];
  const byFinding = new Map(diffs.map((diff) => [diff.findingId, diff]));
  return findings.map((finding) => {
    const diff = byFinding.get(finding.id);
    if (!diff) return finding;
    const section = sections.find((candidate) => candidate.id === finding.sectionId);
    if (!section) return finding;
    const proposedDiff: GapProposedDiff = {
      sectionId: finding.sectionId,
      before: sectionMarkdown(document, finding.sectionId),
      after: diff.after,
    };
    return { ...finding, proposedDiff };
  });
}

function sectionMarkdown(document: ProseMirrorNode, sectionId: string): string {
  let markdown = "";
  document.forEach((node) => {
    if (node.type !== schema.nodes.section || node.attrs["id"] !== sectionId) return;
    markdown = renderMarkdown(schema.nodes.doc!.create(null, node));
  });
  return markdown;
}

interface GapCheckRunRow {
  id: string;
  spec_id: string;
  session_id: string | null;
  semantic_doc_seq: string;
  stopped_at_layer_key: string | null;
  suppressed_count: number;
  matrix: TraceabilityMatrix;
  started_by: string | null;
  created_at: Date;
}

interface GapCheckFindingRow {
  run_id: string;
  finding_id: string;
  ordinal: number;
  kind: GapFinding["kind"];
  severity: GapFindingSeverity;
  layer_key: string;
  section_id: string;
  section_title: string;
  requirement_id: string | null;
  summary: string;
  detail: string;
  proposed_diff: GapProposedDiff | null;
  disposition: GapFindingDisposition;
  open_question_id: string | null;
  disposed_by: string | null;
  disposed_at: Date | null;
}

export class PostgresGapCheckStore implements GapCheckStore {
  constructor(private readonly pool: Pool) {}

  async findRunByFingerprint(specId: string, fingerprint: string): Promise<GapCheckRun | null> {
    const result = await this.pool.query<GapCheckRunRow>(
      `SELECT id, spec_id, session_id, semantic_doc_seq::text, stopped_at_layer_key,
              suppressed_count, matrix, started_by, created_at
         FROM spec_gap_check_run
        WHERE spec_id = $1 AND request_fingerprint = $2`,
      [specId, fingerprint],
    );
    return this.hydrate(result.rows[0]);
  }

  async latestRun(specId: string): Promise<GapCheckRun | null> {
    const result = await this.pool.query<GapCheckRunRow>(
      `SELECT id, spec_id, session_id, semantic_doc_seq::text, stopped_at_layer_key,
              suppressed_count, matrix, started_by, created_at
         FROM spec_gap_check_run
        WHERE spec_id = $1
        ORDER BY created_at DESC, id DESC
        LIMIT 1`,
      [specId],
    );
    return this.hydrate(result.rows[0]);
  }

  async readRun(runId: string): Promise<GapCheckRun | null> {
    const result = await this.pool.query<GapCheckRunRow>(
      `SELECT id, spec_id, session_id, semantic_doc_seq::text, stopped_at_layer_key,
              suppressed_count, matrix, started_by, created_at
         FROM spec_gap_check_run
        WHERE id = $1`,
      [runId],
    );
    return this.hydrate(result.rows[0]);
  }

  async insertRun(run: GapCheckRun, requestFingerprint: string): Promise<void> {
    const client = await this.pool.connect();
    try {
      await client.query("BEGIN");
      const inserted = await client.query(
        `INSERT INTO spec_gap_check_run
           (id, spec_id, session_id, request_fingerprint, semantic_doc_seq,
            stopped_at_layer_key, suppressed_count, matrix, started_by, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         ON CONFLICT (spec_id, request_fingerprint) DO NOTHING`,
        [
          run.id,
          run.specId,
          run.sessionId,
          requestFingerprint,
          run.semanticDocSeq.toString(),
          run.stoppedAtLayerKey,
          run.suppressedCount,
          JSON.stringify(run.matrix),
          run.startedBy,
          run.createdAt,
        ],
      );
      // A concurrent identical request already recorded this pass.
      if (inserted.rowCount === 0) {
        await client.query("ROLLBACK");
        return;
      }
      for (const [ordinal, finding] of run.findings.entries()) {
        await client.query(
          `INSERT INTO spec_gap_check_finding
             (run_id, finding_id, ordinal, kind, severity, layer_key, section_id,
              section_title, requirement_id, summary, detail, proposed_diff, disposition)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, 'pending')`,
          [
            run.id,
            finding.id,
            ordinal,
            finding.kind,
            finding.severity,
            finding.layerKey,
            finding.sectionId,
            finding.sectionTitle,
            finding.requirementId,
            finding.summary,
            finding.detail,
            finding.proposedDiff === null ? null : JSON.stringify(finding.proposedDiff),
          ],
        );
      }
      await client.query("COMMIT");
    } catch (error) {
      await client.query("ROLLBACK");
      throw error;
    } finally {
      client.release();
    }
  }

  async markDisposition(input: {
    runId: string;
    findingId: string;
    disposition: GapFindingDisposition;
    openQuestionId: string | null;
    disposedBy: string | null;
    disposedAt: Date;
  }): Promise<boolean> {
    const result = await this.pool.query(
      `UPDATE spec_gap_check_finding
          SET disposition = $3, open_question_id = $4, disposed_by = $5, disposed_at = $6
        WHERE run_id = $1 AND finding_id = $2 AND disposition = 'pending'`,
      [
        input.runId,
        input.findingId,
        input.disposition,
        input.openQuestionId,
        input.disposedBy,
        input.disposedAt,
      ],
    );
    return (result.rowCount ?? 0) > 0;
  }

  private async hydrate(row: GapCheckRunRow | undefined): Promise<GapCheckRun | null> {
    if (!row) return null;
    const findings = await this.pool.query<GapCheckFindingRow>(
      `SELECT * FROM spec_gap_check_finding WHERE run_id = $1 ORDER BY ordinal`,
      [row.id],
    );
    return {
      id: row.id,
      specId: row.spec_id,
      sessionId: row.session_id,
      semanticDocSeq: BigInt(row.semantic_doc_seq),
      stoppedAtLayerKey: row.stopped_at_layer_key,
      suppressedCount: row.suppressed_count,
      matrix: row.matrix,
      startedBy: row.started_by,
      createdAt: row.created_at,
      findings: findings.rows.map((finding) => ({
        id: finding.finding_id,
        kind: finding.kind,
        severity: finding.severity,
        layerKey: finding.layer_key,
        sectionId: finding.section_id,
        sectionTitle: finding.section_title,
        requirementId: finding.requirement_id as GapFinding["requirementId"],
        summary: finding.summary,
        detail: finding.detail,
        proposedDiff: finding.proposed_diff,
        disposition: finding.disposition,
        openQuestionId: finding.open_question_id,
        disposedBy: finding.disposed_by,
        disposedAt: finding.disposed_at,
      })),
    };
  }
}
