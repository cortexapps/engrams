/** Admission prelude: decide a delivery before the concurrency claim.
 *
 * The concurrency claim is the engine's first side effect on a delivery:
 * under `supersede` it ends the running holder, under `join` it delivers
 * into it. Until 2026-09 the claim came BEFORE the graph's own admission
 * (the built-ins' `facts` code block + `admit` filter), so a delivery the
 * graph would filter a step later had already superseded a live run: any
 * comment on a PR under review — "LGTM", `@engrams stop` — killed the
 * review, and the stop path could never end a run `halted`.
 *
 * The prelude (`admissionPrelude`: the entrypoint's leading `code` +
 * `filter` blocks) is pure, so dispatch evaluates it here against the same
 * scope the run would see. A rejection records a `filtered` run row with
 * no workflow and no claim; an admission proceeds to the claim. A prelude
 * that throws (a code error) admits — the run then fails visibly on the
 * same block instead of a delivery vanishing.
 *
 * Instance-bound runs (ADR 0120) evaluate it too, against the workstream's
 * input snapshot layered over the automation's inputs — the same inputs
 * loadSnapshot hands the run.
 */

import type { RunSnapshot } from "./context.ts";
import type { AutomationDefinition } from "./definition.ts";
import type { CodeBlockRuntime } from "./deps.ts";
import { previewDefinition } from "./preview.ts";

export type AdmissionVerdict =
  | { kind: "admit" }
  | { kind: "reject"; blockId: string; reason: string };

export async function evaluateAdmissionPrelude(input: {
  definition: AutomationDefinition;
  inputs: Record<string, unknown>;
  automationId: string;
  automationName: string;
  trigger: RunSnapshot["trigger"];
  aliases: RunSnapshot["aliases"];
  entrypointId: string;
  code: CodeBlockRuntime;
}): Promise<AdmissionVerdict> {
  const res = await previewDefinition({ ...input, preludeOnly: true });
  const rejected = res.blocks.find((b) => b.filterPass === false);
  if (!rejected) return { kind: "admit" };
  return {
    kind: "reject",
    blockId: rejected.blockId,
    reason: `admission: block "${rejected.blockId}" filtered the event`,
  };
}
