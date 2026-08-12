/** Agent tools for collaborative spec documents (ADR 0114 D6 and D11). */

import { z } from "zod";

import {
  SPEC_ALTERNATIVES_MAX_OPTIONS,
  SPEC_ALTERNATIVES_MIN_OPTIONS,
  SPEC_ALTERNATIVES_TRADEOFF_COUNT,
  SPEC_TRADEOFF_SIGNS,
  type SpecAlternativeOption,
  type SpecAlternativesComparison,
  type SpecSelectionSpan,
  type TrackedEditTranscriptChip,
} from "@engrams/spec-document";

import type { ToolContext, ToolRegistry } from "./registry.ts";

const SPEC_TASK_TYPES = ["spec"] as const;

const Revision = z
  .string()
  .regex(/^(0|[1-9][0-9]*)$/, "must be a non-negative decimal revision");
const SectionId = z.string().min(1).max(200);
const ExpectedRevision = Revision.optional().describe(
  "Apply only when the live document is at this revision",
);
const IdempotencyKey = z.string().min(1).max(200);

const ReadInput = z.object({
  section_id: SectionId.optional().describe(
    "Read only this section; omit it to read the full spec",
  ),
});

const SelectionAnchor = z
  .string()
  .min(1)
  .describe("Yjs-relative anchor from the selected document range");
const SelectionFingerprint = z
  .string()
  .regex(/^[0-9a-f]{64}$/, "must be a lower-case SHA-256 fingerprint");

const UpdateSectionInput = z
  .object({
    section_id: SectionId,
    markdown: z
      .string()
      .describe(
        "Replacement Markdown for the section, or for only the selected range when selection anchors are present",
      ),
    selection_start: SelectionAnchor.optional().describe(
      "Start anchor from a selection action; requires every selection field",
    ),
    selection_end: SelectionAnchor.optional().describe(
      "End anchor from a selection action; requires every selection field",
    ),
    selection_text: z
      .string()
      .optional()
      .describe(
        "Selected text for display; structured stale checks use selection_fingerprint",
      ),
    selection_spec_id: z
      .string()
      .uuid()
      .optional()
      .describe("Spec identity captured with the selection"),
    selection_revision: Revision.optional().describe(
      "Document revision captured with the selection",
    ),
    selection_fingerprint: SelectionFingerprint.optional().describe(
      "SHA-256 fingerprint of the complete selected ProseMirror slice and boundaries",
    ),
    expected_rev: ExpectedRevision,
  })
  .superRefine((value, ctx) => {
    const fields = [
      value.selection_start,
      value.selection_end,
      value.selection_text,
      value.selection_spec_id,
      value.selection_revision,
      value.selection_fingerprint,
    ];
    const present = fields.filter((field) => field !== undefined).length;
    if (present !== 0 && present !== fields.length) {
      ctx.addIssue({
        code: "custom",
        path: ["selection_start"],
        message: "all selection fields must be used together",
      });
    }
  });

const SetSectionStateInput = z
  .object({
    section_id: SectionId,
    state: z.enum(["drafted", "confirmed", "n/a"]),
    reason: z
      .string()
      .min(1)
      .max(2_000)
      .optional()
      .describe("Required when state is n/a"),
    expected_rev: ExpectedRevision,
  })
  .superRefine((value, ctx) => {
    if (value.state === "n/a" && value.reason === undefined) {
      ctx.addIssue({
        code: "custom",
        path: ["reason"],
        message: 'required when state is "n/a"',
      });
    }
  });

const AddOpenQuestionInput = z.object({
  section_id: SectionId,
  question: z.string().min(1).max(20_000),
  expected_rev: ExpectedRevision,
});

const ResolveOpenQuestionInput = z.object({
  section_id: SectionId,
  question_id: z.string().uuid(),
  answer_markdown: z
    .string()
    .min(1)
    .describe("Answer to add to the section before the question is resolved"),
  expected_rev: ExpectedRevision,
});

const UpdateBlockInput = z.object({
  section_id: SectionId,
  block_id: z.string().min(1).max(200),
  source: z
    .string()
    .describe("Replacement source specification for the diagram block"),
  expected_rev: ExpectedRevision,
});

const OptionKey = z
  .string()
  .min(1)
  .max(40)
  .describe("Short card label, for example A");

const AlternativeTradeoff = z.object({
  sign: z.enum(SPEC_TRADEOFF_SIGNS).describe("+ for a gain, - for a cost, ~ for a caveat"),
  text: z.string().min(1).max(400),
});

const AlternativeOption = z.object({
  key: OptionKey,
  title: z.string().min(1).max(200).describe("The one-line premise on the card"),
  tradeoffs: z
    .array(AlternativeTradeoff)
    .length(SPEC_ALTERNATIVES_TRADEOFF_COUNT)
    .describe("Exactly three signed trade-off lines"),
});

const ComparisonCell = z.object({
  option_key: OptionKey,
  value: z.string().min(1).max(400),
});

const ComparisonRow = z.object({
  axis: z.string().min(1).max(200),
  cells: z.array(ComparisonCell).min(SPEC_ALTERNATIVES_MIN_OPTIONS).max(SPEC_ALTERNATIVES_MAX_OPTIONS),
});

const ProposeAlternativesInput = z.object({
  section_id: SectionId.describe("The alternatives section this set belongs to"),
  options: z.array(AlternativeOption).min(SPEC_ALTERNATIVES_MIN_OPTIONS).max(SPEC_ALTERNATIVES_MAX_OPTIONS),
  comparison_provenance: z
    .string()
    .min(1)
    .max(500)
    .describe("One caption that covers the comparison, for example a verified file and commit"),
  comparison_rows: z
    .array(ComparisonRow)
    .min(1)
    .max(20)
    .describe("Numbers against numbers; one value for every option on each axis"),
  lean_key: OptionKey.optional().describe("The option you lean towards; omit when you have none"),
  expected_rev: ExpectedRevision,
});

const DecideAlternativeInput = z.object({
  set_id: z.string().min(1).max(200).describe("The set_id returned by spec_propose_alternatives"),
  option_key: OptionKey.optional().describe(
    "The winning card; omit it when the author chose a hybrid that no card holds",
  ),
  reason: z.string().min(1).max(4_000).describe("Why the winner won; written into the section"),
  expected_rev: ExpectedRevision,
});

const UpdateNotesInput = z.object({
  markdown: z.string().describe("Replacement working notes as Markdown"),
  expected_rev: ExpectedRevision,
});

const TicketProposal = z.object({
  client_id: z
    .string()
    .min(1)
    .max(200)
    .describe("Stable identifier within this proposal"),
  parent_client_id: z.string().min(1).max(200).optional(),
  title: z.string().min(1).max(500),
  description: z.string().min(1),
  section_id: SectionId,
  depends_on: z.array(z.string().min(1).max(200)).optional(),
});

const ProposeTicketsInput = z.object({
  tickets: z.array(TicketProposal).min(1).max(500),
  idempotency_key: IdempotencyKey,
  expected_rev: ExpectedRevision,
});

const ReadOutput = z.object({
  spec_id: z.string().uuid(),
  rev: Revision,
  markdown: z.string(),
  section_id: SectionId.optional(),
});

const MutationOutput = z.object({
  applied: z.boolean(),
  new_rev: Revision,
  concurrent_editors: z.array(z.string()),
  transcript_chip: z
    .object({
      kind: z.literal("spec_tracked_edit"),
      specId: z.string().uuid(),
      sectionId: SectionId,
      before: z.string(),
      after: z.string(),
    })
    .optional(),
  checkpoint_id: z.string().uuid().optional(),
});

const ProposeAlternativesOutput = z.object({
  set_id: z.string(),
  applied: z.boolean(),
  new_rev: Revision,
  concurrent_editors: z.array(z.string()),
});

export interface SpecReference {
  id: string;
}
export interface LiveSpecRead {
  specId: string;
  rev: bigint;
  markdown: string;
  sectionId?: string;
}

export interface SpecMutationResult {
  applied: boolean;
  newRev: bigint;
  concurrentEditors: string[];
  transcriptChip?: TrackedEditTranscriptChip;
  checkpointId?: string;
}

export interface SpecAlternativesProposalResult extends SpecMutationResult {
  setId: string;
}

export interface SpecMutationContext {
  actorUserId?: string;
  sessionId: string;
  toolCallId: string;
  expectedRev?: bigint;
}

export interface SpecToolDocumentService {
  read(specId: string, sectionId?: string): Promise<LiveSpecRead>;
  updateSection(
    specId: string,
    input: SpecMutationContext & {
      sectionId: string;
      markdown: string;
      selection?: SpecSelectionSpan;
    },
  ): Promise<SpecMutationResult>;
  setSectionState(
    specId: string,
    input: SpecMutationContext & {
      sectionId: string;
      state: "drafted" | "confirmed" | "n/a";
      reason?: string;
    },
  ): Promise<SpecMutationResult>;
  addOpenQuestion(
    specId: string,
    input: SpecMutationContext & { sectionId: string; question: string },
  ): Promise<SpecMutationResult>;
  resolveOpenQuestion(
    specId: string,
    input: SpecMutationContext & {
      sectionId: string;
      questionId: string;
      answerMarkdown: string;
    },
  ): Promise<SpecMutationResult>;
  updateBlock(
    specId: string,
    input: SpecMutationContext & {
      sectionId: string;
      blockId: string;
      source: string;
    },
  ): Promise<SpecMutationResult>;
  proposeAlternatives(
    specId: string,
    input: SpecMutationContext & {
      sectionId: string;
      options: SpecAlternativeOption[];
      comparison: SpecAlternativesComparison;
      leanKey: string | null;
    },
  ): Promise<SpecAlternativesProposalResult>;
  decideAlternative(
    specId: string,
    input: SpecMutationContext & {
      setId: string;
      optionKey: string | null;
      reason: string;
    },
  ): Promise<SpecMutationResult>;
  updateNotes(
    specId: string,
    input: SpecMutationContext & { markdown: string },
  ): Promise<SpecMutationResult>;
  proposeTickets(
    specId: string,
    input: SpecMutationContext & {
      idempotencyKey: string;
      tickets: z.output<typeof TicketProposal>[];
    },
  ): Promise<SpecMutationResult>;
}

export interface SpecProjectionRefresh {
  request(input: {
    specId: string;
    sessionId: string;
    source: "agent-tool-mutation";
  }): Promise<void>;
}

export interface SpecAgentPresence {
  enter(input: {
    specId: string;
    sessionId: string;
    toolCallId: string;
    sectionId: string;
  }): Promise<void>;
  leave(input: {
    specId: string;
    sessionId: string;
    toolCallId: string;
  }): Promise<void>;
}

export interface SpecToolDeps {
  resolveSpecForSession(sessionId: string): Promise<SpecReference | null>;
  documents: SpecToolDocumentService;
  projection: SpecProjectionRefresh;
  presence: SpecAgentPresence;
}

function mutationContext(
  ctx: ToolContext,
  expectedRev: string | undefined,
): SpecMutationContext {
  return {
    ...(ctx.userId === undefined ? {} : { actorUserId: ctx.userId }),
    sessionId: ctx.sessionId,
    toolCallId: ctx.toolCallId,
    ...(expectedRev === undefined ? {} : { expectedRev: BigInt(expectedRev) }),
  };
}

function mutationOutput(
  result: SpecMutationResult,
): z.input<typeof MutationOutput> {
  return {
    applied: result.applied,
    new_rev: result.newRev.toString(),
    concurrent_editors: result.concurrentEditors,
    ...(result.transcriptChip === undefined
      ? {}
      : { transcript_chip: result.transcriptChip }),
    ...(result.checkpointId === undefined ? {} : { checkpoint_id: result.checkpointId }),
  };
}

function updateSelection(
  args: z.output<typeof UpdateSectionInput>,
): SpecSelectionSpan | undefined {
  if (args.selection_start === undefined) return undefined;
  if (
    args.selection_end === undefined ||
    args.selection_text === undefined ||
    args.selection_spec_id === undefined ||
    args.selection_revision === undefined ||
    args.selection_fingerprint === undefined
  ) {
    throw new Error("The parsed selection is incomplete.");
  }
  return {
    specId: args.selection_spec_id,
    sectionId: args.section_id,
    revision: args.selection_revision,
    startAnchor: args.selection_start,
    endAnchor: args.selection_end,
    selectedText: args.selection_text,
    sliceFingerprint: args.selection_fingerprint,
  };
}

async function requireSpec(
  ctx: ToolContext,
  deps: SpecToolDeps,
): Promise<SpecReference> {
  const spec = await deps.resolveSpecForSession(ctx.sessionId);
  if (spec === null)
    throw new Error("this session is not linked to a spec document");
  return spec;
}

async function withSectionPresence<T>(
  ctx: ToolContext,
  deps: SpecToolDeps,
  specId: string,
  sectionId: string,
  action: () => Promise<T>,
): Promise<T> {
  const presence = {
    specId,
    sessionId: ctx.sessionId,
    toolCallId: ctx.toolCallId,
  };
  await deps.presence.enter({ ...presence, sectionId });
  try {
    return await action();
  } finally {
    await deps.presence.leave(presence);
  }
}

async function finishMutation(
  ctx: ToolContext,
  deps: SpecToolDeps,
  specId: string,
  result: SpecMutationResult,
): Promise<z.input<typeof MutationOutput>> {
  if (result.applied) {
    await deps.projection.request({
      specId,
      sessionId: ctx.sessionId,
      source: "agent-tool-mutation",
    });
  }
  return mutationOutput(result);
}

/** Register the orchestrator-handled, synchronous spec tool family. */
export function registerSpecTools(
  registry: ToolRegistry,
  deps: SpecToolDeps,
): void {
  registry.register({
    name: "spec_read",
    taskTypes: SPEC_TASK_TYPES,
    description:
      "Read the live spec document, or one section, without using the file projection.",
    input: ReadInput,
    output: ReadOutput,
    handling: "handled",
    execution: "sync",
    handler: async (ctx, args) => {
      const spec = await requireSpec(ctx, deps);
      const read = () => deps.documents.read(spec.id, args.section_id);
      const result =
        args.section_id === undefined
          ? await read()
          : await withSectionPresence(
              ctx,
              deps,
              spec.id,
              args.section_id,
              read,
            );
      return {
        spec_id: result.specId,
        rev: result.rev.toString(),
        markdown: result.markdown,
        ...(result.sectionId === undefined
          ? {}
          : { section_id: result.sectionId }),
      };
    },
  });

  registry.register({
    name: "spec_update_section",
    taskTypes: SPEC_TASK_TYPES,
    description:
      "Replace one spec section, or only an anchored selected range, with Markdown parsed by the document service. For a selection action, copy every selection field exactly. The structured fingerprint confines the edit to the original ProseMirror slice.",
    input: UpdateSectionInput,
    output: MutationOutput,
    handling: "handled",
    execution: "sync",
    handler: async (ctx, args) => {
      const spec = await requireSpec(ctx, deps);
      const selection = updateSelection(args);
      const result = await withSectionPresence(
        ctx,
        deps,
        spec.id,
        args.section_id,
        () =>
          deps.documents.updateSection(spec.id, {
            ...mutationContext(ctx, args.expected_rev),
            sectionId: args.section_id,
            markdown: args.markdown,
            ...(selection === undefined ? {} : { selection }),
          }),
      );
      return finishMutation(ctx, deps, spec.id, result);
    },
  });

  registry.register({
    name: "spec_set_section_state",
    taskTypes: SPEC_TASK_TYPES,
    description:
      "Set a section to drafted, confirmed, or n/a. The n/a state requires a reason.",
    input: SetSectionStateInput,
    output: MutationOutput,
    handling: "handled",
    execution: "sync",
    handler: async (ctx, args) => {
      const spec = await requireSpec(ctx, deps);
      const result = await withSectionPresence(
        ctx,
        deps,
        spec.id,
        args.section_id,
        () =>
          deps.documents.setSectionState(spec.id, {
            ...mutationContext(ctx, args.expected_rev),
            sectionId: args.section_id,
            state: args.state,
            ...(args.reason === undefined ? {} : { reason: args.reason }),
          }),
      );
      return finishMutation(ctx, deps, spec.id, result);
    },
  });

  registry.register({
    name: "spec_add_open_question",
    taskTypes: SPEC_TASK_TYPES,
    description: "Add an open question anchored to a spec section.",
    input: AddOpenQuestionInput,
    output: MutationOutput,
    handling: "handled",
    execution: "sync",
    handler: async (ctx, args) => {
      const spec = await requireSpec(ctx, deps);
      const result = await withSectionPresence(
        ctx,
        deps,
        spec.id,
        args.section_id,
        () =>
          deps.documents.addOpenQuestion(spec.id, {
            ...mutationContext(ctx, args.expected_rev),
            sectionId: args.section_id,
            question: args.question,
          }),
      );
      return finishMutation(ctx, deps, spec.id, result);
    },
  });

  registry.register({
    name: "spec_resolve_open_question",
    taskTypes: SPEC_TASK_TYPES,
    description:
      "Resolve an open question and add its answer to the anchored section in one document mutation.",
    input: ResolveOpenQuestionInput,
    output: MutationOutput,
    handling: "handled",
    execution: "sync",
    handler: async (ctx, args) => {
      const spec = await requireSpec(ctx, deps);
      const result = await withSectionPresence(
        ctx,
        deps,
        spec.id,
        args.section_id,
        () =>
          deps.documents.resolveOpenQuestion(spec.id, {
            ...mutationContext(ctx, args.expected_rev),
            sectionId: args.section_id,
            questionId: args.question_id,
            answerMarkdown: args.answer_markdown,
          }),
      );
      return finishMutation(ctx, deps, spec.id, result);
    },
  });

  registry.register({
    name: "spec_update_block",
    taskTypes: SPEC_TASK_TYPES,
    description:
      "Replace the source specification of a diagram block in one section.",
    input: UpdateBlockInput,
    output: MutationOutput,
    handling: "handled",
    execution: "sync",
    handler: async (ctx, args) => {
      const spec = await requireSpec(ctx, deps);
      const result = await withSectionPresence(
        ctx,
        deps,
        spec.id,
        args.section_id,
        () =>
          deps.documents.updateBlock(spec.id, {
            ...mutationContext(ctx, args.expected_rev),
            sectionId: args.section_id,
            blockId: args.block_id,
            source: args.source,
          }),
      );
      return finishMutation(ctx, deps, spec.id, result);
    },
  });

  registry.register({
    name: "spec_propose_alternatives",
    taskTypes: SPEC_TASK_TYPES,
    description:
      "Propose two or three alternatives as cards in the canvas. Each card carries a one-line premise and exactly three signed trade-off lines (+ gain, - cost, ~ caveat). The comparison holds the numbers, under one provenance caption that says what you verified them against. This stores the set; it does not write the section.",
    input: ProposeAlternativesInput,
    output: ProposeAlternativesOutput,
    handling: "handled",
    execution: "sync",
    handler: async (ctx, args) => {
      const spec = await requireSpec(ctx, deps);
      const result = await withSectionPresence(ctx, deps, spec.id, args.section_id, () =>
        deps.documents.proposeAlternatives(spec.id, {
          ...mutationContext(ctx, args.expected_rev),
          sectionId: args.section_id,
          options: args.options.map((option) => ({
            key: option.key,
            title: option.title,
            tradeoffs: option.tradeoffs.map((tradeoff) => ({
              sign: tradeoff.sign,
              text: tradeoff.text,
            })),
          })),
          comparison: {
            provenance: args.comparison_provenance,
            rows: args.comparison_rows.map((row) => ({
              axis: row.axis,
              cells: row.cells.map((cell) => ({
                optionKey: cell.option_key,
                value: cell.value,
              })),
            })),
          },
          leanKey: args.lean_key ?? null,
        }),
      );
      return {
        set_id: result.setId,
        applied: result.applied,
        new_rev: result.newRev.toString(),
        concurrent_editors: result.concurrentEditors,
      };
    },
  });

  registry.register({
    name: "spec_decide_alternative",
    taskTypes: SPEC_TASK_TYPES,
    description:
      "Record the pick for a proposed alternatives set and write the alternatives section with every option, the comparison, and the reason the winner won. Omit option_key when the author chose a hybrid that no card holds.",
    input: DecideAlternativeInput,
    output: MutationOutput,
    handling: "handled",
    execution: "sync",
    handler: async (ctx, args) => {
      const spec = await requireSpec(ctx, deps);
      const result = await deps.documents.decideAlternative(spec.id, {
        ...mutationContext(ctx, args.expected_rev),
        setId: args.set_id,
        optionKey: args.option_key ?? null,
        reason: args.reason,
      });
      return finishMutation(ctx, deps, spec.id, result);
    },
  });

  registry.register({
    name: "spec_update_notes",
    taskTypes: SPEC_TASK_TYPES,
    description: "Replace the collaborative working notes pane with Markdown.",
    input: UpdateNotesInput,
    output: MutationOutput,
    handling: "handled",
    execution: "sync",
    handler: async (ctx, args) => {
      const spec = await requireSpec(ctx, deps);
      const result = await deps.documents.updateNotes(spec.id, {
        ...mutationContext(ctx, args.expected_rev),
        markdown: args.markdown,
      });
      return finishMutation(ctx, deps, spec.id, result);
    },
  });

  registry.register({
    name: "spec_propose_tickets",
    taskTypes: SPEC_TASK_TYPES,
    description:
      "Replace the post-publish ticket proposal tree. Each ticket must link to a spec section.",
    input: ProposeTicketsInput,
    output: MutationOutput,
    handling: "handled",
    execution: "sync",
    handler: async (ctx, args) => {
      const spec = await requireSpec(ctx, deps);
      const result = await deps.documents.proposeTickets(spec.id, {
        ...mutationContext(ctx, args.expected_rev),
        idempotencyKey: args.idempotency_key,
        tickets: args.tickets,
      });
      return finishMutation(ctx, deps, spec.id, result);
    },
  });
}
