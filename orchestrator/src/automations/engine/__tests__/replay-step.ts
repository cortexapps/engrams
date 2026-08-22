/** A step runner that behaves like DBOS recovery (test helper, ADR 0119).
 *
 * The unit harnesses' `step: (fn) => fn()` cannot see a replay bug: every
 * step closure always runs, so state kept in process memory "works". Real
 * DBOS does not re-run a checkpointed step's closure after a pod restart —
 * it returns the recorded output — and `recv` re-delivers the messages the
 * dead pod already received. Any state that only lives inside a closure or
 * a module-level map is therefore gone after recovery.
 *
 * This runner journals every step output and every received message on the
 * first pass. `crash()` makes the next `recv` hang forever (the pod died
 * while waiting), and `restart()` switches to replay: the journal is walked
 * in order, step closures are NOT invoked, recorded outputs and messages
 * are handed back, and once the journal is exhausted the run goes live again
 * from the next undelivered message. A step name that differs from the
 * journal at the same position is a determinism bug and throws.
 */

import type { EngineReceiver, EngineStepRunner } from "../deps.ts";
import type { AutomationInbox } from "../inbox.ts";

type Entry =
  | { kind: "step"; name: string; value: unknown }
  | { kind: "recv"; value: AutomationInbox | null };

/** A scripted message, or `"crash"` = the pod dies before this message. */
export type ReplayScript = Array<AutomationInbox | null | "crash">;

export interface ReplayRunner {
  step: EngineStepRunner;
  recv: EngineReceiver;
  /** Simulate the pod coming back: replay the journal, then go live. */
  restart(): void;
  /** Step names in execution order across both passes (live and replayed). */
  readonly names: string[];
  /** Names of the steps whose closure actually ran (never a replayed one). */
  readonly executed: string[];
  /** True while a replayed step/recv is being handed back. */
  readonly replaying: boolean;
}

export function makeReplayRunner(script: ReplayScript): ReplayRunner {
  const journal: Entry[] = [];
  const queue = [...script];
  const names: string[] = [];
  const executed: string[] = [];
  let cursor = journal.length; // == journal.length means live
  const never = new Promise<AutomationInbox | null>(() => {});

  const step: EngineStepRunner = async <T>(fn: () => Promise<T>, name: string): Promise<T> => {
    names.push(name);
    if (cursor < journal.length) {
      const entry = journal[cursor]!;
      cursor += 1;
      if (entry.kind !== "step" || entry.name !== name) {
        throw new Error(
          `replay: expected ${entry.kind === "step" ? `step ${entry.name}` : "recv"}, got step ${name}`,
        );
      }
      return entry.value as T;
    }
    const value = await fn();
    executed.push(name);
    journal.push({ kind: "step", name, value });
    cursor = journal.length;
    return value;
  };

  const recv: EngineReceiver = async () => {
    if (cursor < journal.length) {
      const entry = journal[cursor]!;
      cursor += 1;
      if (entry.kind !== "recv") throw new Error(`replay: expected recv, got step ${entry.name}`);
      return entry.value;
    }
    const next = queue.shift();
    if (next === "crash") return never;
    const value = next ?? null;
    journal.push({ kind: "recv", value });
    cursor = journal.length;
    return value;
  };

  return {
    step,
    recv,
    restart() {
      cursor = 0;
    },
    names,
    executed,
    get replaying() {
      return cursor < journal.length;
    },
  };
}
