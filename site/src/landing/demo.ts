// The invented run the hero animates. Shared by the page (first render) and
// the client script (every tick), so the two never drift apart. Session ids,
// PR numbers, and timings are illustrative.

export type Node = { x: number; y: number; kind: string; label: string; t: string };
export const nodes: Node[] = [
  { x: 0, y: 0, kind: "trigger", label: "Schedule · 06:00", t: "cron" },
  { x: 265, y: 0, kind: "block", label: "Create session", t: "0.8s" },
  { x: 520, y: 0, kind: "block", label: "Send prompt", t: "—" },
  { x: 520, y: 160, kind: "wait", label: "Wait for agent", t: "4m 12s" },
  { x: 520, y: 320, kind: "shell", label: "Run tests", t: "exit 0" },
  { x: 265, y: 320, kind: "branch", label: "Failures fixed?", t: "yes" },
  { x: 0, y: 320, kind: "github", label: "Open pull request", t: "#4821" },
];

/** Edge i lights up once step > i; the loop edge (index 6) lights at step >= 7. */
export const edges = [
  "M200 32 H265",
  "M465 32 H520",
  "M620 64 V160",
  "M620 224 V320",
  "M520 352 H465",
  "M265 352 H200",
  "M100 320 V224 M100 224 V64",
];

export const logs = [
  "06:00:00  trigger fired · schedule",
  "06:00:01  session se_9f3ea71c restored from base snapshot · 0.8s",
  '06:00:01  prompt → "find and fix the flaky tests in ci"',
  "06:04:13  agent idle · 3 files changed",
  "06:04:15  $ npm test  ·  exit 0  ·  148 passed",
  "06:04:15  branch → yes",
  "06:04:16  PR #4821 opened · run complete · snapshot written (1.2 MiB)",
];

export const RUN_START = 1042;
export const CHUNKS = 320;

/** Deterministic pseudo-random owner per chunk cell: 0 is the base image,
 * 1..39 are sessions, so the field fills in the same order on every visit. */
export function chunkOwner(i: number): number {
  const x = Math.sin(i * 9301 + 49297) * 233280;
  return Math.floor((x - Math.floor(x)) * 40);
}

/** The state of every animated element at a given tick and step. */
export function frame(tick: number, step: number) {
  const sessions = 1 + (tick % 24);
  const phase = tick % 3;
  const chunks = Array.from({ length: CHUNKS }, (_, i) => {
    const owner = chunkOwner(i);
    if (owner === 0) return "base";
    if (owner <= sessions) return owner === sessions && phase < 2 ? "writing" : "delta";
    return "";
  });
  const deltaMiB = Math.round(sessions * 1.6 * 10) / 10;
  const visibleLogs = logs.slice(Math.max(0, Math.min(step, 7) - 3), Math.min(step, 7));
  return {
    sessions,
    stored: `4 GiB + ${deltaMiB} MiB`,
    chunks,
    visibleLogs,
    complete: step >= 7,
  };
}
