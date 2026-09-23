// The invented run the hero animates and the board rotates. Shared by the
// page (first render) and the client script (every tick), so the two never
// drift apart. Session ids, PR numbers, and timings are illustrative.

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
  "06:04:13  agent idle · transcript 212 events · 3 files changed",
  "06:04:15  $ npm test  ·  exit 0  ·  148 passed",
  "06:04:15  branch → yes",
  "06:04:16  PR #4821 opened · run complete · snapshot written (1.2 MiB)",
];

export type Run = { time: string; name: string; trigger: string; dur: string; status: "done" | "running" | "waiting" | "queued" };
export const runs: Run[] = [
  { time: "06:00", name: "dependency-upgrades", trigger: "schedule", dur: "12m 02s", status: "done" },
  { time: "06:12", name: "pr-review · #4821", trigger: "pull request", dur: "1m 48s", status: "done" },
  { time: "06:30", name: "memory-hot-spots", trigger: "schedule", dur: "9m 40s", status: "done" },
  { time: "07:03", name: "slack · #eng-infra", trigger: "thread mention", dur: "waiting", status: "waiting" },
  { time: "08:15", name: "pr-review · #4823", trigger: "pull request", dur: "0m 51s", status: "running" },
  { time: "08:20", name: "bug-triage", trigger: "webhook · datadog", dur: "2m 30s", status: "running" },
  { time: "08:22", name: "project-owner", trigger: "linear issue", dur: "—", status: "queued" },
];

export const glyph: Record<Run["status"], string> = { done: "●", running: "◐", waiting: "◌", queued: "○" };

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
  const rot = Math.floor(tick / 3) % runs.length;
  const board = runs.map((_, i) => runs[(i + rot) % runs.length]!);
  const visibleLogs = logs.slice(Math.max(0, Math.min(step, 7) - 3), Math.min(step, 7));
  return {
    sessions,
    stored: `4 GiB + ${deltaMiB} MiB`,
    chunks,
    board,
    boardCount: 41 + (tick % 7),
    visibleLogs,
    complete: step >= 7,
  };
}
