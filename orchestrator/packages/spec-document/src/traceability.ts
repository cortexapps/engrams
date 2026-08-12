import {
  extractRequirementDefinitions,
  extractRequirementReferences,
  type RequirementDefinition,
  type RequirementId,
} from "./requirements.ts";

/** The template section key that holds the requirement ledger. */
export const REQUIREMENTS_SECTION_KEY = "requirements";

/**
 * The shortest block that carries a claim. A shorter block is a stub, a label
 * or a placeholder, and flagging it as scope creep would only add noise.
 */
export const SUBSTANTIVE_BLOCK_MIN_CHARS = 40;

export type TraceabilitySectionState = "empty" | "drafted" | "confirmed" | "n/a";

export interface TraceabilityLayer {
  key: string;
  title: string;
}

/** One top-level block of a section body, in document order. */
export interface TraceabilityBlock {
  index: number;
  text: string;
}

export interface TraceabilitySection {
  /** The stable section node id. Findings anchor to it. */
  id: string;
  key: string;
  title: string;
  layerKey: string;
  state: TraceabilitySectionState;
  blocks: readonly TraceabilityBlock[];
}

export interface TraceabilityInput {
  /** Template layers, outermost first. */
  layers: readonly TraceabilityLayer[];
  /** Document sections, in document order. */
  sections: readonly TraceabilitySection[];
  requirementsSectionKey?: string;
}

export type GapFindingKind =
  | "requirement_gap"
  | "scope_creep"
  | "speculative_machinery"
  | "missing_outer_layer"
  | "no_requirements"
  | "red_team";

export type GapFindingSeverity = "fatal" | "gap" | "note";

/** A replacement the person may accept. The pass never applies one itself (R28). */
export interface GapProposedDiff {
  sectionId: string;
  before: string;
  after: string;
}

export interface GapFinding {
  /** Stable across a repeated pass over the same document. */
  id: string;
  kind: GapFindingKind;
  severity: GapFindingSeverity;
  layerKey: string;
  sectionId: string;
  sectionTitle: string;
  requirementId: RequirementId | null;
  summary: string;
  detail: string;
  proposedDiff: GapProposedDiff | null;
}

export interface TraceabilityCitation {
  sectionId: string;
  sectionTitle: string;
  blockIndex: number;
}

export interface TraceabilityMatrixCell {
  layerKey: string;
  covered: boolean;
  citations: readonly TraceabilityCitation[];
  /** The amber cell text. Null when the cell has nothing to report. */
  note: string | null;
}

export type TraceabilityVerdict = "covered" | "gap" | "scope";

export interface TraceabilityMatrixRow {
  /** Null on a row that reports content citing no requirement. */
  requirementId: RequirementId | null;
  label: string;
  verdict: TraceabilityVerdict;
  cells: readonly TraceabilityMatrixCell[];
}

export interface TraceabilityMatrix {
  /** The matrix columns: every layer below the requirement ledger. */
  layers: readonly TraceabilityLayer[];
  rows: readonly TraceabilityMatrixRow[];
}

export interface TraceabilityResult {
  matrix: TraceabilityMatrix;
  findings: readonly GapFinding[];
}

export class TraceabilityInputError extends Error {
  constructor(
    readonly code: "missing_requirements_section" | "unknown_layer",
    message: string,
  ) {
    super(message);
    this.name = "TraceabilityInputError";
  }
}

function isSubstantive(block: TraceabilityBlock): boolean {
  return block.text.trim().length >= SUBSTANTIVE_BLOCK_MIN_CHARS;
}

function sectionText(section: TraceabilitySection): string {
  return section.blocks.map((block) => block.text).join("\n");
}

/** Shorten a block for a one-line finding summary. */
function excerpt(text: string, limit = 90): string {
  const normalized = text.trim().replace(/\s+/g, " ");
  return normalized.length <= limit ? normalized : `${normalized.slice(0, limit - 1)}…`;
}

/**
 * Read the traceability of one spec document: requirement coverage, content
 * that cites no requirement, and the structural layer check. The pass reads
 * only; every finding is a proposal for a person to dispose of (R28).
 */
export function analyzeTraceability(input: TraceabilityInput): TraceabilityResult {
  const requirementsKey = input.requirementsSectionKey ?? REQUIREMENTS_SECTION_KEY;
  const layerIndexes = new Map(input.layers.map((layer, index) => [layer.key, index]));
  for (const section of input.sections) {
    if (!layerIndexes.has(section.layerKey)) {
      throw new TraceabilityInputError(
        "unknown_layer",
        `Spec section ${section.id} names an unknown layer: ${section.layerKey}`,
      );
    }
  }

  const ledgerSection = input.sections.find((section) => section.key === requirementsKey);
  if (!ledgerSection) {
    throw new TraceabilityInputError(
      "missing_requirements_section",
      "The gap check needs a requirements section to trace against.",
    );
  }
  const ledgerLayerIndex = layerIndexes.get(ledgerSection.layerKey)!;

  const definitions = extractRequirementDefinitions(sectionText(ledgerSection));
  const live = definitions.filter((definition) => !definition.tombstone);
  const liveIds = new Set(live.map((definition) => definition.id));

  const downstreamLayers = input.layers.slice(ledgerLayerIndex + 1);
  const downstreamSections = input.sections.filter(
    (section) => layerIndexes.get(section.layerKey)! > ledgerLayerIndex,
  );
  // An n/a section is deliberately out of scope, so it neither covers a
  // requirement nor owes one a citation.
  const tracedSections = downstreamSections.filter((section) => section.state !== "n/a");

  const citations = collectCitations(tracedSections, liveIds);
  const findings: GapFinding[] = [];
  const rows: TraceabilityMatrixRow[] = [];

  for (const definition of live) {
    const { row, gaps } = requirementRow(definition, downstreamLayers, tracedSections, citations);
    rows.push(row);
    findings.push(...gaps);
  }

  const uncited = uncitedContent(tracedSections, downstreamLayers, liveIds, layerIndexes);
  rows.push(...uncited.rows);
  findings.push(...uncited.findings);

  findings.push(
    ...structuralFindings(input, ledgerSection, ledgerLayerIndex, layerIndexes, live.length),
  );

  return { matrix: { layers: downstreamLayers, rows }, findings };
}

/** requirement id → the citations of it, in document order. */
function collectCitations(
  sections: readonly TraceabilitySection[],
  liveIds: ReadonlySet<RequirementId>,
): Map<RequirementId, TraceabilityCitation[]> {
  const byRequirement = new Map<RequirementId, TraceabilityCitation[]>();
  for (const section of sections) {
    for (const block of section.blocks) {
      for (const reference of extractRequirementReferences(block.text)) {
        if (!liveIds.has(reference.id)) continue;
        const entries = byRequirement.get(reference.id) ?? [];
        const already = entries.some(
          (entry) => entry.sectionId === section.id && entry.blockIndex === block.index,
        );
        if (!already) {
          entries.push({
            sectionId: section.id,
            sectionTitle: section.title,
            blockIndex: block.index,
          });
        }
        byRequirement.set(reference.id, entries);
      }
    }
  }
  return byRequirement;
}

function requirementRow(
  definition: RequirementDefinition,
  downstreamLayers: readonly TraceabilityLayer[],
  tracedSections: readonly TraceabilitySection[],
  citations: ReadonlyMap<RequirementId, readonly TraceabilityCitation[]>,
): { row: TraceabilityMatrixRow; gaps: GapFinding[] } {
  const all = citations.get(definition.id) ?? [];
  const sectionLayer = new Map(tracedSections.map((section) => [section.id, section.layerKey]));
  const cells: TraceabilityMatrixCell[] = [];
  const gaps: GapFinding[] = [];

  for (const layer of downstreamLayers) {
    const inLayer = all.filter((citation) => sectionLayer.get(citation.sectionId) === layer.key);
    if (inLayer.length > 0) {
      cells.push({ layerKey: layer.key, covered: true, citations: inLayer, note: null });
      continue;
    }
    const note = `no ${layer.title} content cites ${definition.id}`;
    cells.push({ layerKey: layer.key, covered: false, citations: [], note });
    // Anchor the gap at the first traced section of the layer, because that is
    // where the missing coverage belongs.
    const anchor = tracedSections.find((section) => section.layerKey === layer.key);
    if (!anchor) continue;
    gaps.push({
      id: `requirement_gap:${definition.id}:${layer.key}`,
      kind: "requirement_gap",
      severity: "gap",
      layerKey: layer.key,
      sectionId: anchor.id,
      sectionTitle: anchor.title,
      requirementId: definition.id,
      summary: `${definition.id} has no ${layer.title} coverage`,
      detail: `${definition.id} — ${definition.text ?? ""} — is not cited anywhere in ${layer.title}.`,
      proposedDiff: null,
    });
  }

  return {
    row: {
      requirementId: definition.id,
      label: definition.text ?? definition.id,
      verdict: cells.every((cell) => cell.covered) ? "covered" : "gap",
      cells,
    },
    gaps,
  };
}

function uncitedContent(
  tracedSections: readonly TraceabilitySection[],
  downstreamLayers: readonly TraceabilityLayer[],
  liveIds: ReadonlySet<RequirementId>,
  layerIndexes: ReadonlyMap<string, number>,
): { rows: TraceabilityMatrixRow[]; findings: GapFinding[] } {
  const rows: TraceabilityMatrixRow[] = [];
  const findings: GapFinding[] = [];
  const firstDownstreamKey = downstreamLayers[0]?.key;

  for (const section of tracedSections) {
    for (const block of section.blocks) {
      if (!isSubstantive(block)) continue;
      const cited = extractRequirementReferences(block.text).some((reference) =>
        liveIds.has(reference.id),
      );
      if (cited) continue;

      // The first layer below the ledger states behavior; a deeper layer states
      // the machinery that must trace back to a behavior or an N#.
      const contract = section.layerKey === firstDownstreamKey;
      const kind: GapFindingKind = contract ? "scope_creep" : "speculative_machinery";
      const summary = contract
        ? `${section.title} states behavior that cites no requirement`
        : `${section.title} adds machinery that cites no requirement`;
      const note = `${section.title} ¶${block.index + 1} "${excerpt(block.text, 48)}" cites no requirement`;
      const detail = contract
        ? `${section.title} ¶${block.index + 1} — "${excerpt(block.text)}" — cites no requirement. Add a requirement, or cut it.`
        : `${section.title} ¶${block.index + 1} — "${excerpt(block.text)}" — traces to no behavior and to no N#. Justify it, or cut it.`;

      findings.push({
        id: `${kind}:${section.id}:${block.index}`,
        kind,
        severity: "gap",
        layerKey: section.layerKey,
        sectionId: section.id,
        sectionTitle: section.title,
        requirementId: null,
        summary,
        detail,
        proposedDiff: null,
      });

      rows.push({
        requirementId: null,
        label: excerpt(block.text, 60),
        verdict: "scope",
        cells: downstreamLayers.map((layer) => ({
          layerKey: layer.key,
          covered: false,
          citations: [],
          note: layer.key === section.layerKey ? note : null,
        })),
      });
    }
  }

  // Report the outer layers first, so the matrix reads outside-in like the
  // findings list does.
  rows.sort(
    (left, right) =>
      firstNoteLayerIndex(left, layerIndexes) - firstNoteLayerIndex(right, layerIndexes),
  );
  return { rows, findings };
}

function firstNoteLayerIndex(
  row: TraceabilityMatrixRow,
  layerIndexes: ReadonlyMap<string, number>,
): number {
  for (const cell of row.cells) {
    if (cell.note !== null) return layerIndexes.get(cell.layerKey) ?? 0;
  }
  return 0;
}

/**
 * The layer model is the point (R31). A spec that states machinery on top of an
 * empty layer above it fails here, structurally, before any detail review.
 */
function structuralFindings(
  input: TraceabilityInput,
  ledgerSection: TraceabilitySection,
  ledgerLayerIndex: number,
  layerIndexes: ReadonlyMap<string, number>,
  liveRequirementCount: number,
): GapFinding[] {
  const findings: GapFinding[] = [];
  const substantiveLayers = new Set<string>();
  for (const section of input.sections) {
    if (section.state === "n/a") continue;
    if (section.blocks.some(isSubstantive)) substantiveLayers.add(section.layerKey);
  }

  let deepest = -1;
  input.layers.forEach((layer, index) => {
    if (substantiveLayers.has(layer.key)) deepest = index;
  });

  const ledgerLayerKey = input.layers[ledgerLayerIndex]!.key;
  const requirementsMissing = liveRequirementCount === 0 && deepest > ledgerLayerIndex;
  if (requirementsMissing) {
    findings.push({
      id: `no_requirements:${ledgerSection.id}`,
      kind: "no_requirements",
      severity: "fatal",
      layerKey: ledgerSection.layerKey,
      sectionId: ledgerSection.id,
      sectionTitle: ledgerSection.title,
      requirementId: null,
      summary: `${ledgerSection.title} states no requirement, but deeper layers are written`,
      detail:
        "The spec describes design with no requirement to trace it to. Every deeper layer rests on a premise that is not written down. State the requirements before the pass continues.",
      proposedDiff: null,
    });
  }

  for (let index = 0; index < deepest; index += 1) {
    const layer = input.layers[index]!;
    if (substantiveLayers.has(layer.key)) continue;
    // The dedicated no_requirements finding already reports this layer.
    if (requirementsMissing && layer.key === ledgerLayerKey) continue;
    const anchor = input.sections.find((section) => section.layerKey === layer.key) ?? ledgerSection;
    findings.push({
      id: `missing_outer_layer:${layer.key}`,
      kind: "missing_outer_layer",
      severity: "fatal",
      layerKey: layer.key,
      sectionId: anchor.id,
      sectionTitle: anchor.title,
      requirementId: null,
      summary: `${layer.title} is empty, but a deeper layer is written`,
      detail: `${layer.title} carries no content, yet ${input.layers[deepest]!.title} is drafted on top of it. Fill the outer layer first; the work below it rests on an unstated premise.`,
      proposedDiff: null,
    });
  }

  findings.sort(
    (left, right) => layerIndexes.get(left.layerKey)! - layerIndexes.get(right.layerKey)!,
  );
  return findings;
}

const SEVERITY_RANK: Record<GapFindingSeverity, number> = { fatal: 0, gap: 1, note: 2 };

export interface OutsideInResult {
  findings: readonly GapFinding[];
  /** The layer that halted the pass, or null when the pass ran to the end. */
  stoppedAtLayerKey: string | null;
  /** Findings dropped because they sit below the halting layer. */
  suppressedCount: number;
}

/**
 * Order findings outside-in and stop the pass at a fatal flaw (R32). A fatal
 * finding in an outer layer is reported first, and every finding below it is
 * withheld — polishing a deeper layer of a broken premise is wasted work.
 */
export function applyOutsideIn(
  findings: readonly GapFinding[],
  layers: readonly TraceabilityLayer[],
): OutsideInResult {
  const layerIndexes = new Map(layers.map((layer, index) => [layer.key, index]));
  const indexOf = (finding: GapFinding): number =>
    layerIndexes.get(finding.layerKey) ?? layers.length;

  const ordered = [...findings].sort((left, right) => {
    const bySeverity = SEVERITY_RANK[left.severity] - SEVERITY_RANK[right.severity];
    if (bySeverity !== 0) return bySeverity;
    const byLayer = indexOf(left) - indexOf(right);
    if (byLayer !== 0) return byLayer;
    return left.id.localeCompare(right.id);
  });

  const fatal = ordered.filter((finding) => finding.severity === "fatal");
  if (fatal.length === 0) {
    return { findings: ordered, stoppedAtLayerKey: null, suppressedCount: 0 };
  }

  const outermost = fatal.reduce((best, finding) =>
    indexOf(finding) < indexOf(best) ? finding : best,
  );
  const stopIndex = indexOf(outermost);
  const kept = ordered.filter((finding) => indexOf(finding) <= stopIndex);
  return {
    findings: kept,
    stoppedAtLayerKey: outermost.layerKey,
    suppressedCount: ordered.length - kept.length,
  };
}
