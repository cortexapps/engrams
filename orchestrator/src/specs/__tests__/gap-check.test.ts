import { describe, expect, test } from "bun:test";
import { schema, SPEC_FRAGMENT_NAME } from "@engrams/spec-document";
import type { Node as ProseMirrorNode } from "prosemirror-model";
import { prosemirrorToYXmlFragment } from "y-prosemirror";
import * as Y from "yjs";

import type { GapFindingDisposition, SpecTemplateLayer, SpecTemplateSection } from "../../db/schema.ts";
import type { SpecMutationContext, SpecMutationResult } from "../../tools/specs.ts";
import type { SpecRailMetadata, SpecRailStore } from "../../routes/spec-rail.ts";
import type { LoadedSpecDocument } from "../doc-service.ts";
import {
  GapCheckError,
  GapCheckService,
  type GapCheckRun,
  type GapCheckStore,
  type GapCheckDocumentService,
} from "../gap-check.ts";

const SPEC_ID = "00000000-0000-4000-8000-000000000121";
const SESSION_ID = "00000000-0000-4000-8000-000000000122";
const NOW = new Date("2026-08-12T14:31:00.000Z");

const LAYERS: SpecTemplateLayer[] = [
  { key: "intent", title: "Intent" },
  { key: "contract", title: "Contract" },
  { key: "system", title: "System" },
];

function templateSection(key: string, title: string, layerKey: string): SpecTemplateSection {
  return {
    key,
    title,
    layerKey,
    guidance: `Write ${title}.`,
    doneCriteria: [],
    required: true,
    allowNa: true,
  };
}

const TEMPLATE_SECTIONS: SpecTemplateSection[] = [
  templateSection("requirements", "Requirements", "intent"),
  templateSection("behavior", "Behavior", "contract"),
  templateSection("design", "Design", "system"),
];

function specSection(id: string, key: string, title: string, paragraphs: string[]) {
  return schema.nodes.section!.create({ id, templateSectionKey: key }, [
    schema.nodes.sectionHeading!.create(null, schema.text(title)),
    ...(paragraphs.length === 0 ? [""] : paragraphs).map((text) =>
      schema.nodes.paragraph!.create(null, text.length > 0 ? [schema.text(text)] : undefined),
    ),
  ]);
}

/** A spec whose Design layer never cites N1 — one gap, nothing fatal. */
function documentWithOneGap(): ProseMirrorNode {
  return schema.nodes.doc!.create(null, [
    specSection("sec-req", "requirements", "Requirements", [
      "- R1: an org caps its concurrent sandboxes",
      "- N1: the limiter adds under 1ms p99 to session create",
    ]),
    specSection("sec-behavior", "behavior", "Behavior", [
      "Creating a sandbox above the org ceiling is refused with a named reset time (R1).",
      "The refusal states the reset time so a caller can retry deliberately (N1).",
    ]),
    specSection("sec-design", "design", "Design", [
      "A Postgres counter row per org backs the ceiling walk and serves R1 directly.",
    ]),
  ]);
}

function loaded(document: ProseMirrorNode, semanticDocSeq: bigint): LoadedSpecDocument {
  const ydoc = new Y.Doc();
  prosemirrorToYXmlFragment(document, ydoc.getXmlFragment(SPEC_FRAGMENT_NAME));
  return { doc: ydoc, lastAppliedSeq: semanticDocSeq, semanticDocSeq };
}

class MemoryRailStore implements SpecRailStore {
  constructor(private readonly states = new Map<string, { state: "open" | "proposed" | "settled" | "n/a"; naReason: string | null }>()) {}

  async readMetadata(): Promise<SpecRailMetadata | null> {
    return {
      phase: "drafting",
      layers: LAYERS,
      sections: TEMPLATE_SECTIONS,
      states: this.states,
      openQuestionCounts: new Map(),
    };
  }
}

class MemoryGapCheckStore implements GapCheckStore {
  readonly runs = new Map<string, GapCheckRun>();
  private readonly fingerprints = new Map<string, string>();

  async findRunByFingerprint(specId: string, fingerprint: string): Promise<GapCheckRun | null> {
    const id = this.fingerprints.get(`${specId}:${fingerprint}`);
    return id ? (this.runs.get(id) ?? null) : null;
  }

  async latestRun(specId: string): Promise<GapCheckRun | null> {
    const all = [...this.runs.values()].filter((run) => run.specId === specId);
    return all.sort((a, b) => b.createdAt.getTime() - a.createdAt.getTime())[0] ?? null;
  }

  async readRun(runId: string): Promise<GapCheckRun | null> {
    return this.runs.get(runId) ?? null;
  }

  async insertRun(run: GapCheckRun, requestFingerprint: string): Promise<void> {
    const key = `${run.specId}:${requestFingerprint}`;
    if (this.fingerprints.has(key)) return;
    this.fingerprints.set(key, run.id);
    this.runs.set(run.id, run);
  }

  /** Runs once, inside markDisposition, to stage a concurrent disposal. */
  beforeMarkDisposition: (() => Promise<void>) | null = null;

  async markDisposition(input: {
    runId: string;
    findingId: string;
    disposition: GapFindingDisposition;
    openQuestionId: string | null;
    disposedBy: string | null;
    disposedAt: Date;
  }): Promise<boolean> {
    if (this.beforeMarkDisposition) {
      const hook = this.beforeMarkDisposition;
      this.beforeMarkDisposition = null;
      await hook();
    }
    const run = this.runs.get(input.runId);
    if (!run) return false;
    // The real compare-and-swap: only a pending finding can be reserved.
    const current = run.findings.find((finding) => finding.id === input.findingId);
    if (!current || current.disposition !== "pending") return false;
    const findings = run.findings.map((finding) =>
      finding.id === input.findingId
        ? {
            ...finding,
            disposition: input.disposition,
            openQuestionId: input.openQuestionId,
            disposedBy: input.disposedBy,
            disposedAt: input.disposedAt,
          }
        : finding,
    );
    this.runs.set(run.id, { ...run, findings });
    return true;
  }

  async releaseDisposition(runId: string, findingId: string): Promise<void> {
    const run = this.runs.get(runId);
    if (!run) return;
    const findings = run.findings.map((finding) =>
      finding.id === findingId
        ? {
            ...finding,
            disposition: "pending" as const,
            openQuestionId: null,
            disposedBy: null,
            disposedAt: null,
          }
        : finding,
    );
    this.runs.set(run.id, { ...run, findings });
  }
}

interface RecordedCall {
  kind: "addOpenQuestion" | "updateSection";
  sectionId: string;
  payload: string;
  context: SpecMutationContext;
}

class RecordingDocuments implements GapCheckDocumentService {
  readonly calls: RecordedCall[] = [];
  /** Set false to model a revision conflict inside the document services. */
  applied = true;

  async addOpenQuestion(
    _specId: string,
    input: SpecMutationContext & { sectionId: string; question: string },
  ): Promise<SpecMutationResult> {
    this.calls.push({
      kind: "addOpenQuestion",
      sectionId: input.sectionId,
      payload: input.question,
      context: input,
    });
    return { applied: this.applied, newRev: 2n, concurrentEditors: [] };
  }

  async updateSection(
    _specId: string,
    input: SpecMutationContext & { sectionId: string; markdown: string },
  ): Promise<SpecMutationResult> {
    this.calls.push({
      kind: "updateSection",
      sectionId: input.sectionId,
      payload: input.markdown,
      context: input,
    });
    return { applied: this.applied, newRev: 2n, concurrentEditors: [] };
  }
}

function makeService(options: { document?: ProseMirrorNode; rev?: bigint } = {}) {
  const document = options.document ?? documentWithOneGap();
  const rev = options.rev ?? 7n;
  const store = new MemoryGapCheckStore();
  const toolDocuments = new RecordingDocuments();
  const service = new GapCheckService({
    documents: { syncFromLog: async () => loaded(document, rev) },
    railStore: new MemoryRailStore(),
    store,
    toolDocuments,
    now: () => NOW,
  });
  return { service, store, toolDocuments };
}

function runInput(overrides: Partial<Parameters<GapCheckService["run"]>[0]> = {}) {
  return {
    specId: SPEC_ID,
    sessionId: SESSION_ID,
    requestFingerprint: "run-1",
    actorUserId: null,
    ...overrides,
  };
}

describe("GapCheckService.run", () => {
  test("reports a requirement gap and makes zero document edits", async () => {
    const { service, toolDocuments } = makeService();

    const run = await service.run(runInput());

    const gap = run.findings.find((finding) => finding.kind === "requirement_gap")!;
    expect(gap.requirementId).toBe("N1");
    expect(gap.sectionId).toBe("sec-design");
    expect(gap.disposition).toBe("pending");
    expect(run.stoppedAtLayerKey).toBeNull();
    expect(run.semanticDocSeq).toBe(7n);
    // R28: the pass proposes, it never writes.
    expect(toolDocuments.calls).toEqual([]);
  });

  test("renders the matrix with an amber cell for the uncovered layer", async () => {
    const { service } = makeService();

    const run = await service.run(runInput());

    expect(run.matrix.layers.map((layer) => layer.key)).toEqual(["contract", "system"]);
    const row = run.matrix.rows.find((candidate) => candidate.requirementId === "N1")!;
    expect(row.verdict).toBe("gap");
    const cell = row.cells.find((candidate) => candidate.layerKey === "system")!;
    expect(cell.covered).toBe(false);
    expect(cell.note).toBe("no System content cites N1");
  });

  test("a red-team contradiction halts the pass and is reported first", async () => {
    const { service } = makeService();

    const run = await service.run(
      runInput({
        redTeam: [
          {
            layerKey: "contract",
            sectionId: "sec-behavior",
            severity: "fatal",
            summary: "Behavior promises a reset sharper than the gateway cache TTL",
            detail: "Design assumes a 30s cache; Behavior promises a sharper reset. Resolve first.",
          },
        ],
      }),
    );

    expect(run.stoppedAtLayerKey).toBe("contract");
    expect(run.findings[0]!.kind).toBe("red_team");
    expect(run.findings[0]!.severity).toBe("fatal");
    // The System-layer requirement gap is withheld, not lost.
    expect(run.findings.every((finding) => finding.layerKey !== "system")).toBe(true);
    expect(run.suppressedCount).toBe(1);
  });

  test("a red-team finding naming an unknown section is rejected", async () => {
    const { service } = makeService();

    await expect(
      service.run(
        runInput({
          redTeam: [
            {
              layerKey: "contract",
              sectionId: "sec-nowhere",
              severity: "gap",
              summary: "s",
              detail: "d",
            },
          ],
        }),
      ),
    ).rejects.toThrow(GapCheckError);
  });

  test("a replayed request returns the original run", async () => {
    const { service, store } = makeService();

    const first = await service.run(runInput());
    const second = await service.run(runInput());

    expect(second.id).toBe(first.id);
    expect(store.runs.size).toBe(1);
  });

  test("the agent can attach a proposed diff, and before is read from the document", async () => {
    const { service } = makeService();

    const run = await service.run(
      runInput({
        proposedDiffs: [
          {
            findingId: "requirement_gap:N1:system",
            after: "## Design\n\nThe walk is timed with a histogram so N1 stays observable.\n",
          },
        ],
      }),
    );

    const gap = run.findings.find((finding) => finding.id === "requirement_gap:N1:system")!;
    expect(gap.proposedDiff).not.toBeNull();
    expect(gap.proposedDiff!.sectionId).toBe("sec-design");
    expect(gap.proposedDiff!.after).toContain("histogram");
    // Read from the live document, not from the caller.
    expect(gap.proposedDiff!.before).toContain("Postgres counter row per org");
  });
});

describe("GapCheckService.status", () => {
  test("a spec with no pass is stale", async () => {
    const { service } = makeService();

    const status = await service.status(SPEC_ID);

    expect(status.run).toBeNull();
    expect(status.stale).toBe(true);
    expect(status.currentSemanticDocSeq).toBe(7n);
  });

  test("a pass at the current revision is fresh", async () => {
    const { service } = makeService();
    await service.run(runInput());

    const status = await service.status(SPEC_ID);

    expect(status.stale).toBe(false);
    expect(status.run!.semanticDocSeq).toBe(7n);
  });

  test("a pass goes stale when the document moves on", async () => {
    const document = documentWithOneGap();
    const store = new MemoryGapCheckStore();
    let rev = 7n;
    const service = new GapCheckService({
      documents: { syncFromLog: async () => loaded(document, rev) },
      railStore: new MemoryRailStore(),
      store,
      toolDocuments: new RecordingDocuments(),
      now: () => NOW,
    });
    await service.run(runInput());

    rev = 8n;
    const status = await service.status(SPEC_ID);

    expect(status.stale).toBe(true);
    expect(status.run!.semanticDocSeq).toBe(7n);
  });
});

describe("GapCheckService.disposeFinding", () => {
  test("a finding becomes an open question at its own anchor", async () => {
    const { service, toolDocuments } = makeService();
    const run = await service.run(runInput());
    const gap = run.findings.find((finding) => finding.kind === "requirement_gap")!;

    const updated = await service.disposeFinding({
      runId: run.id,
      findingId: gap.id,
      action: "open_question",
      actorUserId: "user-1",
    });

    expect(toolDocuments.calls).toHaveLength(1);
    const call = toolDocuments.calls[0]!;
    expect(call.kind).toBe("addOpenQuestion");
    expect(call.sectionId).toBe(gap.sectionId);
    expect(call.payload).toBe(gap.detail);
    const stored = updated.findings.find((finding) => finding.id === gap.id)!;
    expect(stored.disposition).toBe("question_opened");
    expect(stored.openQuestionId).not.toBeNull();
    expect(stored.disposedBy).toBe("user-1");
  });

  test("accepting a proposed diff replaces the section, and only then", async () => {
    const { service, toolDocuments } = makeService();
    const run = await service.run(
      runInput({
        proposedDiffs: [
          { findingId: "requirement_gap:N1:system", after: "## Design\n\nTimed for N1.\n" },
        ],
      }),
    );
    // Nothing reached the document while the finding sat pending.
    expect(toolDocuments.calls).toEqual([]);

    const updated = await service.disposeFinding({
      runId: run.id,
      findingId: "requirement_gap:N1:system",
      action: "accept_diff",
      actorUserId: "user-1",
    });

    expect(toolDocuments.calls).toHaveLength(1);
    expect(toolDocuments.calls[0]!.kind).toBe("updateSection");
    expect(toolDocuments.calls[0]!.sectionId).toBe("sec-design");
    expect(toolDocuments.calls[0]!.payload).toBe("## Design\n\nTimed for N1.\n");
    expect(
      updated.findings.find((finding) => finding.id === "requirement_gap:N1:system")!.disposition,
    ).toBe("diff_accepted");
  });

  test("a finding with no proposed diff cannot be accepted", async () => {
    const { service, toolDocuments } = makeService();
    const run = await service.run(runInput());
    const gap = run.findings.find((finding) => finding.kind === "requirement_gap")!;

    await expect(
      service.disposeFinding({
        runId: run.id,
        findingId: gap.id,
        action: "accept_diff",
        actorUserId: "user-1",
      }),
    ).rejects.toThrow(GapCheckError);
    expect(toolDocuments.calls).toEqual([]);
  });

  test("a finding is disposed of once", async () => {
    const { service } = makeService();
    const run = await service.run(runInput());
    const gap = run.findings.find((finding) => finding.kind === "requirement_gap")!;
    await service.disposeFinding({
      runId: run.id,
      findingId: gap.id,
      action: "dismiss",
      actorUserId: "user-1",
    });

    await expect(
      service.disposeFinding({
        runId: run.id,
        findingId: gap.id,
        action: "dismiss",
        actorUserId: "user-2",
      }),
    ).rejects.toThrow(GapCheckError);
  });

  test("accepting a diff binds the revision the run read", async () => {
    const { service, toolDocuments } = makeService();
    const run = await service.run(
      runInput({
        proposedDiffs: [
          { findingId: "requirement_gap:N1:system", after: "## Design\n\nTimed for N1.\n" },
        ],
      }),
    );

    await service.disposeFinding({
      runId: run.id,
      findingId: "requirement_gap:N1:system",
      action: "accept_diff",
      actorUserId: "user-1",
    });

    // Without this the whole section is replaced with text written against an
    // older revision, silently discarding any edit made since.
    expect(toolDocuments.calls[0]!.context.expectedRev).toBe(7n);
  });

  test("a section edited since the run refuses the diff and stays pending", async () => {
    const { service, store, toolDocuments } = makeService();
    const run = await service.run(
      runInput({
        proposedDiffs: [
          { findingId: "requirement_gap:N1:system", after: "## Design\n\nTimed for N1.\n" },
        ],
      }),
    );
    // The document moved on, so updateSection reports a conflict.
    toolDocuments.applied = false;

    await expect(
      service.disposeFinding({
        runId: run.id,
        findingId: "requirement_gap:N1:system",
        action: "accept_diff",
        actorUserId: "user-1",
      }),
    ).rejects.toThrow(GapCheckError);

    const stored = await store.readRun(run.id);
    const finding = stored!.findings.find(
      (candidate) => candidate.id === "requirement_gap:N1:system",
    )!;
    // The reservation was released, so somebody can run the check again and
    // dispose of it properly.
    expect(finding.disposition).toBe("pending");
    expect(finding.disposedBy).toBeNull();
  });

  test("a question that does not land leaves the finding pending", async () => {
    const { service, store, toolDocuments } = makeService();
    const run = await service.run(runInput());
    const gap = run.findings.find((finding) => finding.kind === "requirement_gap")!;
    toolDocuments.applied = false;

    await expect(
      service.disposeFinding({
        runId: run.id,
        findingId: gap.id,
        action: "open_question",
        actorUserId: "user-1",
      }),
    ).rejects.toThrow(GapCheckError);

    const stored = await store.readRun(run.id);
    expect(stored!.findings.find((candidate) => candidate.id === gap.id)!.disposition).toBe(
      "pending",
    );
  });

  test("the loser of a concurrent disposal never reaches the document", async () => {
    const { service, store, toolDocuments } = makeService();
    const run = await service.run(
      runInput({
        proposedDiffs: [
          { findingId: "requirement_gap:N1:system", after: "## Design\n\nTimed for N1.\n" },
        ],
      }),
    );
    // Somebody else dismisses the finding between this caller's read and its
    // reservation — the interleaving that "act, then record" gets wrong.
    store.beforeMarkDisposition = async () => {
      await store.markDisposition({
        runId: run.id,
        findingId: "requirement_gap:N1:system",
        disposition: "dismissed",
        openQuestionId: null,
        disposedBy: "user-2",
        disposedAt: NOW,
      });
    };

    await expect(
      service.disposeFinding({
        runId: run.id,
        findingId: "requirement_gap:N1:system",
        action: "accept_diff",
        actorUserId: "user-1",
      }),
    ).rejects.toThrow(GapCheckError);

    // The section was never rewritten, so the record still matches the document.
    expect(toolDocuments.calls).toEqual([]);
    const stored = await store.readRun(run.id);
    const finding = stored!.findings.find(
      (candidate) => candidate.id === "requirement_gap:N1:system",
    )!;
    expect(finding.disposition).toBe("dismissed");
    expect(finding.disposedBy).toBe("user-2");
  });

  test("dismissing a finding never touches the document", async () => {
    const { service, toolDocuments } = makeService();
    const run = await service.run(runInput());
    const gap = run.findings.find((finding) => finding.kind === "requirement_gap")!;

    await service.disposeFinding({
      runId: run.id,
      findingId: gap.id,
      action: "dismiss",
      actorUserId: "user-1",
    });

    expect(toolDocuments.calls).toEqual([]);
  });
});
