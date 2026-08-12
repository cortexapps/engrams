/**
 * The spec session mode (ADR 0114 D6, D7 and D9).
 *
 * The instruction has two parts. The first part is the standing rules of the
 * mode: the tool write path, recon before questions, provenance on every
 * repository statement, and drafting at the frontier. Those rules hold for
 * every spec, so they are constant.
 *
 * The second part comes from the spec's own template snapshot: the layers, the
 * sections with their guidance and done criteria, and the process stage flags.
 * House judgment about process is therefore configuration, not code — an org
 * changes the stages in the template editor, and the next session gets the new
 * instruction. A caller that has no snapshot (an older spec, or a session
 * created before the spec exists) gets the standing rules alone.
 *
 * The prompt stays harness-neutral: it names no agent product and no vendor
 * tool. A test holds that property.
 */

import type {
  SpecTemplateLayer,
  SpecTemplateSection,
  SpecTemplateStageFlags,
  SpecTemplateStageMode,
} from "../db/schema.ts";

/** The part of a spec's template snapshot that shapes the system prompt.
 *  `SpecTemplateSnapshot` satisfies it, so a caller passes the snapshot. */
export interface SpecPromptContext {
  layers: readonly SpecTemplateLayer[];
  sections: readonly SpecTemplateSection[];
  stageFlags: SpecTemplateStageFlags;
}

/** The standing rules of spec mode. Every spec session gets these, with or
 *  without a template snapshot. The live read at the start of each turn avoids
 *  stale disk projections after queued prompts. */
export const SPEC_MODE_SYSTEM_PROMPT = `## Spec mode
When /workspace/spec.md exists, the session has a collaborative spec. At the start of every turn, call spec_read before you reason about or change the spec. spec_read is the live source of truth. Read /workspace/.engrams/spec/digest.md when you need the human-change summary.

Treat /workspace/spec.md as a read-only projection. Never edit it with file or shell tools. Use the spec_* tools for every spec change.

### Start with recon, not with questions
Read the repository before you write your first message. That message states what you found in the repository, and it proposes a shape for the spec. Never open with a list of questions.

Write into the document before you send that message. Use spec_update_section to put the problem statement into the section that holds the problem, and to add the requirements that your recon supports. Mark those requirements as candidates. The person must see reviewable document content first, not chat prose. Then name the largest gap, and ask about it.

### Give provenance for every repository statement
Each statement about the repository carries the file and the short commit sha, in the form \`path/to/file.ts @ 8f2c1a4\`. This rule covers prose, data definitions, interface sketches, and every number that you compare. When you cannot verify a statement, write "unverified" next to it. Never give a guess as a repository fact.

### Draft at the frontier
The frontier is the first layer of the template that the person has not confirmed. Draft there, and keep the later layers short until the frontier moves to them.

When the person gives you content for a later layer, accept it and record it immediately. Mark that content "provisional" until its layer becomes the frontier. Never refuse content because it comes early, and never lose it.`;

/**
 * One process stage from the template flags.
 *
 * A stage is process, and a section is structure. They are independent: a
 * template can keep an "Alternatives" section while its alternatives stage is
 * off. `off` removes the stage text completely — the agent then knows nothing
 * about the stage, which is what the flag means.
 */
interface SpecStage {
  key: keyof SpecTemplateStageFlags;
  title: string;
  /** Trigger sentence when the template runs the stage as part of the process. */
  on: string;
  /** Trigger sentence when the template offers the stage to the person. */
  suggested: string;
  /** What the stage does. Identical for `on` and `suggested`. */
  body: string;
}

const SPEC_STAGES: readonly SpecStage[] = [
  {
    key: "alternatives",
    title: "Alternatives",
    on: "This spec uses the alternatives stage. Run it while the design direction is still open, and do not ask for permission first.",
    suggested:
      "This spec offers the alternatives stage. Tell the person what the stage gives them, and run it only when they accept.",
    body: "In this stage you give at least two credible directions. For each direction, write what it costs and what it gives. Name the direction that you recommend, and give your reason. Write the comparison into the document, not only into the conversation. Every number in the comparison carries provenance.",
  },
  {
    key: "talkItThrough",
    title: "Talk it through",
    on: "This spec uses the talk-it-through stage. Start it when the person wants to think out loud.",
    suggested:
      "This spec offers the talk-it-through stage. Offer it when the person wants to think out loud, and start it only when they accept.",
    body: "In this stage the person speaks freely, and you keep the working notes with spec_update_notes. Write no spec section while the notes are open. Send your complete model each time, with one bullet for each idea, and keep every bullet id stable. Mark each bullet as verified, as contradicted, or as unchecked, and name the receipt for a verified or a contradicted bullet. Keep a contradicted claim beside its receipt. Cluster the bullets by theme, and tag each cluster toward the section where it belongs. Check each repository claim while you listen. A person can rewrite any bullet, and their words win: read the corrections in the reply and correct your model. Call spec_distill_notes when the person asks for the spec, or when the untagged pile stops growing. Distillation writes only the tagged clusters, and a section with no material stays empty.",
  },
  {
    key: "gapCheck",
    title: "Gap check",
    on: "This spec uses the gap-check stage. Run it before the person publishes.",
    suggested:
      "This spec offers the gap-check stage. Offer it before the person publishes, and run it only when they accept.",
    body: "In this stage you compare each required section with its done criteria. Report every gap in one list, and say what closes it. Mark a section as n/a with spec_set_section_state only when the template permits it, and always give the reason. Never report a section as complete while a criterion is open.",
  },
];

/** Assemble the spec-mode instruction for one session. */
export function specModeSystemPrompt(context?: SpecPromptContext): string {
  if (!context) return SPEC_MODE_SYSTEM_PROMPT;
  return [
    SPEC_MODE_SYSTEM_PROMPT,
    structureBlock(context),
    ...stageBlocks(context.stageFlags),
  ].join("\n\n");
}

/** The layers and the sections that this spec owns for its complete lifetime. */
function structureBlock(context: SpecPromptContext): string {
  const layerTitles = new Map(context.layers.map((layer) => [layer.key, layer.title]));
  const lines = [
    "### The structure of this spec",
    "The template below gives the layers and the sections of this spec. The document keeps that structure: you cannot add a section, and you cannot remove a section.",
    "",
    "Layers, from the first to the last:",
    ...context.layers.map(
      (layer, index) =>
        `${index + 1}. ${layer.title}${layer.description ? ` — ${layer.description}` : ""}`,
    ),
    "",
    "Sections:",
  ];
  for (const section of context.sections) {
    lines.push(`- ${section.title} (${sectionFacts(section, layerTitles)}) — ${section.guidance}`);
    if (section.doneCriteria.length > 0) {
      lines.push(`  Done when: ${section.doneCriteria.join(" ")}`);
    }
  }
  return lines.join("\n");
}

function sectionFacts(section: SpecTemplateSection, layerTitles: Map<string, string>): string {
  return [
    `layer: ${layerTitles.get(section.layerKey) ?? section.layerKey}`,
    section.required ? "required" : "optional",
    ...(section.allowNa ? ["n/a is permitted with a reason"] : []),
  ].join("; ");
}

/** One block for each stage that the template runs or offers. An `off` stage
 *  contributes nothing, so the agent never learns that the stage exists. */
function stageBlocks(flags: SpecTemplateStageFlags): string[] {
  const blocks: string[] = [];
  for (const stage of SPEC_STAGES) {
    const trigger = stageTrigger(stage, flags[stage.key]);
    if (!trigger) continue;
    blocks.push([`### ${stage.title} stage`, trigger, stage.body].join("\n"));
  }
  return blocks;
}

function stageTrigger(stage: SpecStage, mode: SpecTemplateStageMode): string | null {
  switch (mode) {
    case "on":
      return stage.on;
    case "suggested":
      return stage.suggested;
    case "off":
      return null;
  }
}
