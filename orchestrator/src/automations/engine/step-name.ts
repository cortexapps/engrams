/** Deterministic step naming (ADR 0119 D2).
 *
 * The frame path names a block's position in the walked graph, including loop
 * iterations. It doubles as `automation_step_run.block_id`, so every
 * iteration and every retry attempt has exactly one ledger row and one DBOS
 * checkpoint. Changing this scheme changes the step contract: bump
 * `ENGINE_STEP_CONTRACT` in the registered workflow body.
 */

export interface Frame {
  blockId: string;
  iteration?: number;
}

export function framePath(frames: readonly Frame[]): string {
  return frames
    .map((f) => (f.iteration === undefined ? f.blockId : `${f.blockId}[${f.iteration}]`))
    .join(".");
}

export function stepName(frames: readonly Frame[], attempt: number): string {
  return `step:${framePath(frames)}:${attempt}`;
}

/** Auxiliary step names that share the contract. */
export const SNAPSHOT_STEP = "step:__snapshot__:0";
export const FINALIZE_STEP = "step:__finalize__:0";

export function conditionStepName(frames: readonly Frame[]): string {
  return `step:${framePath(frames)}.__cond__:0`;
}

export function untilStepName(frames: readonly Frame[], iteration: number): string {
  return `step:${framePath(frames)}[${iteration}].__until__:0`;
}

export function clockStepName(frames: readonly Frame[], n: number): string {
  return `step:${framePath(frames)}:clock:${n}`;
}
