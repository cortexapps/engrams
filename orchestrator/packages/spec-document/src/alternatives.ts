/**
 * The alternatives stage (ADR 0114 D6, requirement R20).
 *
 * Cards and the comparison are structured tool output. They never come from
 * parsed prose. The pick renders one canonical §Alternatives considered body,
 * which the document service writes through the ordinary section write.
 */

/** Two or three cards. More than three outgrows the canvas pane. */
export const SPEC_ALTERNATIVES_MIN_OPTIONS = 2;
export const SPEC_ALTERNATIVES_MAX_OPTIONS = 3;
/** One premise plus exactly three signed trade-off lines. */
export const SPEC_ALTERNATIVES_TRADEOFF_COUNT = 3;

export const SPEC_TRADEOFF_SIGNS = ["+", "-", "~"] as const;
export type SpecTradeoffSign = (typeof SPEC_TRADEOFF_SIGNS)[number];

/** The sign as it is written for a person. Minus is the typographic sign. */
const TRADEOFF_MARKS: Readonly<Record<SpecTradeoffSign, string>> = {
  "+": "+",
  "-": "−",
  "~": "~",
};

export interface SpecAlternativeTradeoff {
  sign: SpecTradeoffSign;
  text: string;
}

export interface SpecAlternativeOption {
  /** Short card label, for example "A". Unique inside one set. */
  key: string;
  /** The one-line premise on the card. */
  title: string;
  tradeoffs: SpecAlternativeTradeoff[];
}

export interface SpecAlternativesComparisonCell {
  optionKey: string;
  value: string;
}

export interface SpecAlternativesComparisonRow {
  axis: string;
  cells: SpecAlternativesComparisonCell[];
}

export interface SpecAlternativesComparison {
  /** One caption that covers the whole comparison surface (R16). */
  provenance: string;
  rows: SpecAlternativesComparisonRow[];
}

/** The agent's proposal: the cards and the numbers behind them. */
export interface AlternativesProposedTranscriptChip {
  kind: "spec_alternatives_proposed";
  specId: string;
  sectionId: string;
  setId: string;
  options: SpecAlternativeOption[];
  comparison: SpecAlternativesComparison;
  /** The option the agent leans towards, or null when it has no lean. */
  leanKey: string | null;
}

/** The pick, or an author-written hybrid. */
export interface AlternativesDecidedTranscriptChip {
  kind: "spec_alternatives_decided";
  specId: string;
  sectionId: string;
  setId: string;
  /** The winning card key, or null for a hybrid that no card holds. */
  pickedKey: string | null;
  /** Why the winner won. This is written into the section. */
  reason: string;
  decidedBy: "agent" | "author";
}

export type SpecAlternativesTranscriptChip =
  | AlternativesProposedTranscriptChip
  | AlternativesDecidedTranscriptChip;

/** What the canvas renders for one spec. */
export interface SpecAlternativesStage {
  proposal: AlternativesProposedTranscriptChip;
  decision: AlternativesDecidedTranscriptChip | null;
}

export class SpecAlternativesError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "SpecAlternativesError";
  }
}

export function isSpecTradeoffSign(value: unknown): value is SpecTradeoffSign {
  return SPEC_TRADEOFF_SIGNS.includes(value as SpecTradeoffSign);
}

export interface SpecAlternativesProposal {
  options: SpecAlternativeOption[];
  comparison: SpecAlternativesComparison;
  leanKey?: string | null;
}

/**
 * Enforce the card ceiling before anything is stored. The card shape is the
 * contract the canvas renders against, so a bad set fails at the tool, not in
 * the browser.
 */
export function validateSpecAlternatives(proposal: SpecAlternativesProposal): void {
  const { options, comparison } = proposal;
  if (
    options.length < SPEC_ALTERNATIVES_MIN_OPTIONS ||
    options.length > SPEC_ALTERNATIVES_MAX_OPTIONS
  ) {
    throw new SpecAlternativesError(
      `An alternatives set needs ${SPEC_ALTERNATIVES_MIN_OPTIONS} or ${SPEC_ALTERNATIVES_MAX_OPTIONS} options.`,
    );
  }
  const keys = new Set<string>();
  for (const option of options) {
    if (option.key.trim().length === 0) {
      throw new SpecAlternativesError("Every option needs a card key.");
    }
    if (keys.has(option.key)) {
      throw new SpecAlternativesError(`Option key ${option.key} is used twice.`);
    }
    keys.add(option.key);
    if (option.title.trim().length === 0) {
      throw new SpecAlternativesError(`Option ${option.key} needs a one-line premise.`);
    }
    if (option.tradeoffs.length !== SPEC_ALTERNATIVES_TRADEOFF_COUNT) {
      throw new SpecAlternativesError(
        `Option ${option.key} needs exactly ${SPEC_ALTERNATIVES_TRADEOFF_COUNT} trade-off lines.`,
      );
    }
    for (const tradeoff of option.tradeoffs) {
      if (!isSpecTradeoffSign(tradeoff.sign)) {
        throw new SpecAlternativesError(
          `Option ${option.key} has a trade-off sign that is not +, -, or ~.`,
        );
      }
      if (tradeoff.text.trim().length === 0) {
        throw new SpecAlternativesError(`Option ${option.key} has an empty trade-off line.`);
      }
    }
  }
  if (comparison.provenance.trim().length === 0) {
    throw new SpecAlternativesError("The comparison needs a provenance caption.");
  }
  if (comparison.rows.length === 0) {
    throw new SpecAlternativesError("The comparison needs at least one axis.");
  }
  for (const row of comparison.rows) {
    if (row.axis.trim().length === 0) {
      throw new SpecAlternativesError("Every comparison row needs an axis name.");
    }
    const covered = new Set(row.cells.map((cell) => cell.optionKey));
    for (const cell of row.cells) {
      if (!keys.has(cell.optionKey)) {
        throw new SpecAlternativesError(
          `Comparison axis "${row.axis}" names an unknown option: ${cell.optionKey}.`,
        );
      }
    }
    if (covered.size !== keys.size || row.cells.length !== keys.size) {
      throw new SpecAlternativesError(
        `Comparison axis "${row.axis}" must give one value for every option.`,
      );
    }
  }
  const lean = proposal.leanKey ?? null;
  if (lean !== null && !keys.has(lean)) {
    throw new SpecAlternativesError(`The lean names an unknown option: ${lean}.`);
  }
}

/**
 * Render §Alternatives considered: every option, the comparison with its
 * provenance caption, and the reason the winner won.
 *
 * The spec schema has paragraphs, H3 headings and code blocks only, so the
 * comparison is rendered as one axis line per row rather than as a table.
 */
export function renderAlternativesConsidered(
  proposal: AlternativesProposedTranscriptChip,
  decision: AlternativesDecidedTranscriptChip,
): string {
  const winner =
    decision.pickedKey === null
      ? null
      : (proposal.options.find((option) => option.key === decision.pickedKey) ?? null);
  if (decision.pickedKey !== null && winner === null) {
    throw new SpecAlternativesError(`The decision names an unknown option: ${decision.pickedKey}.`);
  }
  const blocks: string[] = [
    `${proposal.options.length} options were considered. ${proposal.comparison.provenance}`,
  ];
  for (const option of proposal.options) {
    blocks.push(`### ${option.key} · ${option.title}`);
    blocks.push(
      option.tradeoffs
        .map((tradeoff) => `${TRADEOFF_MARKS[tradeoff.sign]} ${tradeoff.text}`)
        .join("\n"),
    );
  }
  blocks.push("### Comparison");
  blocks.push(proposal.comparison.provenance);
  blocks.push(
    proposal.comparison.rows
      .map(
        (row) =>
          `${row.axis} — ${row.cells.map((cell) => `${cell.optionKey}: ${cell.value}`).join(" · ")}`,
      )
      .join("\n"),
  );
  blocks.push(
    winner === null ? "### Selected: a hybrid" : `### Selected: ${winner.key} · ${winner.title}`,
  );
  blocks.push(decision.reason);
  return `${blocks.join("\n\n")}\n`;
}
