import { createHash } from "node:crypto";
import {
  closeSync,
  existsSync,
  fsyncSync,
  lstatSync,
  openSync,
  readFileSync,
  renameSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";

interface EffectEvent {
  sequence: number;
  callHash: string;
}
interface EffectState {
  version: 1;
  executions: number;
  count: number;
  events: EffectEvent[];
}
const hash = (value: string) =>
  createHash("sha256").update(value).digest("hex");
const exactKeys = (value: object, keys: string[]) =>
  Object.keys(value).sort().join() === keys.sort().join();

// Each execution commits before the protocol fixture can send or hold a response.
// Duplicate IDs deliberately increment again so replay remains visible.
export function openEffects(root: string) {
  const path = join(root, "effects.json");
  const next = join(root, "effects.next");
  if (existsSync(next)) throw new Error("Incomplete durable state");
  let state: EffectState = { version: 1, executions: 0, count: 0, events: [] };
  if (existsSync(path)) {
    try {
      if (!lstatSync(path).isFile() || lstatSync(path).size > 16384) throw 0;
      const parsed = JSON.parse(readFileSync(path, "utf8"));
      const s = parsed.state;
      if (
        !exactKeys(parsed, ["state", "sha256"]) ||
        !s ||
        !exactKeys(s, ["version", "executions", "count", "events"]) ||
        parsed.sha256 !== hash(JSON.stringify(s)) ||
        s.version !== 1 ||
        !Number.isSafeInteger(s.executions) ||
        s.executions < 0 ||
        !Number.isSafeInteger(s.count) ||
        !Array.isArray(s.events) ||
        s.events.length !== Math.min(s.executions, 64) ||
        s.events.some(
          (e: EffectEvent, i: number) =>
            !e ||
            !exactKeys(e, ["sequence", "callHash"]) ||
            e.sequence !== s.executions - s.events.length + i + 1 ||
            !/^[a-f0-9]{64}$/.test(e.callHash),
        )
      )
        throw 0;
      state = s;
    } catch {
      throw new Error("Invalid durable state");
    }
  }
  let failed = false;
  function save(value: EffectState) {
    const fd = openSync(next, "wx", 0o600);
    try {
      writeFileSync(
        fd,
        JSON.stringify({ state: value, sha256: hash(JSON.stringify(value)) }),
      );
      fsyncSync(fd);
    } finally {
      closeSync(fd);
    }
    renameSync(next, path);
    const dir = openSync(root, "r");
    try {
      fsyncSync(dir);
    } finally {
      closeSync(dir);
    }
  }
  if (!existsSync(path)) save(state);
  return {
    healthy: () => !failed,
    snapshot: () => structuredClone(state),
    commit(id: string, delta: number) {
      if (failed) throw new Error("Durable state unavailable");
      const count = state.count + delta;
      const executions = state.executions + 1;
      if (!Number.isSafeInteger(count) || !Number.isSafeInteger(executions))
        throw new Error("Counter limit");
      const value: EffectState = {
        version: 1,
        executions,
        count,
        events: [
          ...state.events.slice(-63),
          { sequence: executions, callHash: hash(id) },
        ],
      };
      try {
        save(value);
      } catch {
        failed = true;
        throw new Error("Durable commit failed");
      }
      state = value;
      return count;
    },
  };
}
