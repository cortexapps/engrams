/**
 * The spec session mode (ADR 0114 D6, D7 and D9).
 *
 * The instruction has two parts. The first part is the standing rules of the
 * mode: the tool write path, recon before questions, provenance on every
 * repository statement, and the document-first workflow. Those rules hold
 * for every spec, so they are constant.
 *
 * The second part comes from the spec's own template snapshot: the layers and
 * the sections with their guidance and done criteria. A caller that has no
 * snapshot gets the standing rules alone.
 *
 * The prompt stays harness-neutral: it names no agent product and no vendor
 * tool. A test holds that property.
 */

import type { SpecTemplateLayer, SpecTemplateSection } from "../db/schema.ts";

/** The part of a spec's template snapshot that shapes the system prompt.
 *  `SpecTemplateSnapshot` satisfies it, so a caller passes the snapshot. */
export interface SpecPromptContext {
  layers: readonly SpecTemplateLayer[];
  sections: readonly SpecTemplateSection[];
}

/** The standing rules of spec mode. Every spec session gets these, with or
 *  without a template snapshot. Freshness is pushed, not polled (ADR 0114 D9):
 *  a UserPromptSubmit hook delivers the digest with each prompt, and every
 *  content mutation carries expected_rev, so a stale section write bounces
 *  instead of clobbering. The agent therefore reads the sections the digest
 *  names instead of the whole spec every turn. */
export const SPEC_MODE_SYSTEM_PROMPT = `## Spec mode
When /workspace/spec.md exists, the session has a collaborative spec. Each prompt arrives with a digest that names the sections humans changed since your last turn. Call spec_read with a section_id before you write into or reason from a section the digest names, or one you have not read in this session. Call spec_read without a section_id when you need the whole document. spec_read is the live source of truth; /workspace/spec.md is a projection that can lag behind it.

Every spec_update_section, spec_set_section_state, and spec_update_block call carries expected_rev: the rev from your latest spec_read of that section. A result with applied=false means the section changed after that read. Re-read the section, then reapply your change on top of what you find. Edits elsewhere in the document never bounce your write, so never retry a bounced call with the returned rev without reading first.

Treat /workspace/spec.md as a read-only projection. Never edit it with file or shell tools. Use the spec_* tools for every spec change.

### Start with recon, not with questions
Read the repository before you write your first message. That message states what you found in the repository, and it proposes a shape for the spec. Never open with a list of questions.

Write into the document before you send that message. Use spec_update_section to put the problem statement into the section that holds the problem, and to add the requirements that your recon supports. Mark those requirements as candidates. The person must see reviewable document content first, not chat prose. Then name the largest gap, and ask about it.

### Give provenance for every repository statement
Each statement about the repository carries the file and the short commit sha, in the form \`path/to/file.ts @ 8f2c1a4\`. This rule covers prose, data definitions, interface sketches, and every number that you compare. When you cannot verify a statement, write "unverified" next to it. Never give a guess as a repository fact.`;

/** Assemble the spec-mode instruction for one session. */
export function specModeSystemPrompt(context?: SpecPromptContext): string {
  if (!context) return SPEC_MODE_SYSTEM_PROMPT;
  return [SPEC_MODE_SYSTEM_PROMPT, structureBlock(context)].join("\n\n");
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
