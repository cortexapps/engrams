/** The client-side block registry for the automation editor (ADR 0119 phase 3).
 *
 * Hybrid: most block kinds render through a generic typed-field form described
 * by `fields`; a few ship a custom inspector. Block `config` shapes mirror the
 * orchestrator's zod schemas in `engine/blocks/*.ts` — the server remains the
 * validation authority and returns `BlockError{block_id, field}` which the
 * editor routes back to these fields.
 */

import type { LucideIcon } from "lucide-react";
import {
  Braces,
  CircleStop,
  Filter,
  FilePlus2,
  GitBranch,
  Hourglass,
  Lock,
  MailOpen,
  Play,
  Repeat,
  Send,
  Terminal,
  Zap,
} from "lucide-react";

export type BlockConfig = Record<string, unknown>;

export interface BlockDef {
  id: string;
  type: string;
  retry?: { attempts: number; retryOn?: "transient" | "always" | "never" };
  /** Fields users may override on a built-in; absent/empty = nothing tunable. */
  tunable?: string[];
  config: BlockConfig;
  then?: BlockDef[];
  else?: BlockDef[];
  body?: BlockDef[];
}

export interface TriggerSpec {
  kind: "cron" | "webhook" | "integration" | "manual";
  [key: string]: unknown;
}

export interface AutomationDefinition {
  engine: number;
  trigger: TriggerSpec;
  blocks: BlockDef[];
  inputsSchema: unknown[];
  settings: {
    concurrency?: { keyTemplate: string; policy: "queue" | "supersede" | "skip" | "join" };
    runDeadlineSeconds?: number;
    endSessionsOnFinish: boolean;
  };
}

export type FieldSpec =
  | { type: "template"; key: string; label: string; multiline?: boolean; help?: string }
  | { type: "string"; key: string; label: string; help?: string }
  | { type: "number"; key: string; label: string; help?: string; min?: number; max?: number }
  | { type: "boolean"; key: string; label: string; help?: string }
  | { type: "duration"; key: string; label: string; help?: string }
  | { type: "session_ref"; key: string; label: string; help?: string }
  | { type: "select"; key: string; label: string; options: readonly string[]; help?: string }
  | { type: "profile"; key: string; label: string; help?: string }
  | { type: "secret_ref"; key: string; label: string; help?: string }
  | { type: "json"; key: string; label: string; help?: string };

export interface BlockKindSpec {
  kind: string;
  label: string;
  description: string;
  icon: LucideIcon;
  /** One-line row summary from the block config. */
  summary(config: BlockConfig): string;
  /** Generic-form fields. Absent when a custom inspector owns the block. */
  fields?: FieldSpec[];
  /** Custom inspector id — resolved by BlockInspector. */
  inspector?:
    | "create_session"
    | "branch"
    | "loop"
    | "filter"
    | "code"
    | "integration_action"
    | "system";
  /** Whether this kind nests child lists (branch: then/else; loop: body). */
  nests?: "branch" | "loop";
  /** Kinds only a built-in may reference. */
  system?: boolean;
  /** Fresh default config when inserted. */
  defaults(): BlockConfig;
}

function str(value: unknown, fallback = ""): string {
  return typeof value === "string" ? value : fallback;
}

function truncate(value: string, max = 60): string {
  return value.length > max ? `${value.slice(0, max - 1)}…` : value;
}

function sessionRefLabel(value: unknown): string {
  if (typeof value === "object" && value !== null) {
    const ref = value as { blockId?: unknown; template?: unknown };
    if (typeof ref.blockId === "string") return `session from “${ref.blockId}”`;
    if (typeof ref.template === "string") return `session ${ref.template}`;
  }
  return "session (unset)";
}

const WAIT_DEADLINE_FIELD: FieldSpec = {
  type: "duration",
  key: "deadlineSeconds",
  label: "Wait deadline",
  help: "How long to wait before the run ends with a deadline outcome.",
};

export const BLOCK_KINDS: readonly BlockKindSpec[] = [
  {
    kind: "filter",
    label: "Filter",
    description: "Continue only when the conditions hold; otherwise the run ends as filtered.",
    icon: Filter,
    inspector: "filter",
    summary: (c) => {
      const group = c["conditions"] as { conditions?: unknown[] } | undefined;
      const n = Array.isArray(group?.conditions) ? group.conditions.length : 0;
      return n === 0 ? "No conditions (always passes)" : `${n} condition${n === 1 ? "" : "s"}`;
    },
    defaults: () => ({ conditions: { mode: "all", conditions: [] } }),
  },
  {
    kind: "branch",
    label: "Branch",
    description: "Run the then-list when the conditions hold, else the else-list.",
    icon: GitBranch,
    inspector: "branch",
    nests: "branch",
    summary: (c) => {
      const group = c["conditions"] as { conditions?: unknown[] } | undefined;
      const n = Array.isArray(group?.conditions) ? group.conditions.length : 0;
      return `${n} condition${n === 1 ? "" : "s"}`;
    },
    defaults: () => ({ conditions: { mode: "all", conditions: [] } }),
  },
  {
    kind: "loop",
    label: "Loop",
    description: "Repeat the body until a condition holds or the iteration cap is reached.",
    icon: Repeat,
    inspector: "loop",
    nests: "loop",
    summary: (c) =>
      `up to ${typeof c["maxIterations"] === "number" ? c["maxIterations"] : "?"} iterations`,
    defaults: () => ({ maxIterations: 10 }),
  },
  {
    kind: "code",
    label: "Code",
    description: "Run JavaScript in the sandbox to compute a value or a yes/no.",
    icon: Braces,
    inspector: "code",
    summary: (c) => (c["mode"] === "boolean" ? "JavaScript predicate" : "JavaScript value"),
    defaults: () => ({
      source: "export default ({ event, inputs, steps, trigger }) => {\n  return true;\n};\n",
      mode: "value",
    }),
  },
  {
    kind: "create_session",
    label: "Create session",
    description: "Start a session from a profile with an initial prompt.",
    icon: Play,
    inspector: "create_session",
    summary: (c) => {
      const profile = str(c["profileId"]);
      const prompt = truncate(str(c["promptTemplate"]).split("\n")[0] ?? "", 48);
      return profile ? `${profile}${prompt ? ` · ${prompt}` : ""}` : "No profile selected";
    },
    defaults: () => ({ profileId: "", promptTemplate: "", includeEventContext: false }),
  },
  {
    kind: "send_prompt",
    label: "Send prompt",
    description: "Send a prompt to a session and optionally wait for it to finish.",
    icon: Send,
    fields: [
      { type: "session_ref", key: "session", label: "Session" },
      { type: "template", key: "promptTemplate", label: "Prompt", multiline: true },
      {
        type: "select",
        key: "waitFor.kind",
        label: "Wait for",
        options: ["run_end", "signal", "none"],
        help: "run_end: the session goes idle · signal: a named signal from the session · none: fire and forget",
      },
      {
        type: "string",
        key: "waitFor.name",
        label: "Signal name",
        help: "Only for wait = signal.",
      },
      WAIT_DEADLINE_FIELD,
      { type: "string", key: "harnessMode", label: "Harness mode", help: "e.g. plan (optional)" },
    ],
    summary: (c) => {
      const wait = c["waitFor"] as { kind?: string; name?: string } | undefined;
      const w =
        wait?.kind === "signal"
          ? `wait for ${wait.name ?? "signal"}`
          : wait?.kind === "none"
            ? "no wait"
            : "wait for run end";
      return `${sessionRefLabel(c["session"])} · ${w}`;
    },
    defaults: () => ({
      session: { blockId: "" },
      promptTemplate: "",
      waitFor: { kind: "run_end" },
    }),
  },
  {
    kind: "wait_session",
    label: "Wait for session",
    description: "Park until the session goes idle or ends.",
    icon: Hourglass,
    fields: [
      { type: "session_ref", key: "session", label: "Session" },
      { type: "select", key: "until", label: "Until", options: ["idle", "ended"] },
      WAIT_DEADLINE_FIELD,
    ],
    summary: (c) => `${sessionRefLabel(c["session"])} · until ${str(c["until"], "idle")}`,
    defaults: () => ({ session: { blockId: "" }, until: "idle" }),
  },
  {
    kind: "wait_event",
    label: "Wait for event",
    description: "Receive the next delivery joined into this run (join concurrency).",
    icon: MailOpen,
    fields: [
      {
        type: "json",
        key: "eventKeys",
        label: "Event keys",
        help: "JSON array of event keys; empty = any.",
      },
      WAIT_DEADLINE_FIELD,
    ],
    summary: (c) => {
      const keys = c["eventKeys"];
      return Array.isArray(keys) && keys.length > 0 ? `next ${keys.join(", ")}` : "next event";
    },
    defaults: () => ({}),
  },
  {
    kind: "end_session",
    label: "End session",
    description: "Tear down a session this run created.",
    icon: CircleStop,
    fields: [{ type: "session_ref", key: "session", label: "Session" }],
    summary: (c) => `end ${sessionRefLabel(c["session"])}`,
    defaults: () => ({ session: { blockId: "" } }),
  },
  {
    kind: "run_command",
    label: "Run command",
    description: "Run a shell command inside a session and capture its output.",
    icon: Terminal,
    fields: [
      { type: "session_ref", key: "session", label: "Session" },
      { type: "template", key: "commandTemplate", label: "Command", multiline: true },
      { type: "number", key: "deadlineMs", label: "Deadline (ms)", min: 1000, max: 600000 },
      { type: "boolean", key: "allowNonZeroExit", label: "Allow non-zero exit" },
    ],
    summary: (c) => truncate(str(c["commandTemplate"]).split("\n")[0] ?? "", 60) || "No command",
    defaults: () => ({ session: { blockId: "" }, commandTemplate: "" }),
  },
  {
    kind: "write_files",
    label: "Write files",
    description: "Write rendered files into a session's filesystem.",
    icon: FilePlus2,
    fields: [
      { type: "session_ref", key: "session", label: "Session" },
      {
        type: "json",
        key: "files",
        label: "Files",
        help: 'JSON array of {"path": "/abs/path", "contentTemplate": "…", "mode": 420}',
      },
    ],
    summary: (c) => {
      const files = c["files"];
      return Array.isArray(files)
        ? `${files.length} file${files.length === 1 ? "" : "s"}`
        : "No files";
    },
    defaults: () => ({ session: { blockId: "" }, files: [] }),
  },
  {
    kind: "integration_action",
    label: "Integration action",
    description: "Call a catalog action on a connected integration with org credentials.",
    icon: Zap,
    inspector: "integration_action",
    summary: (c) => {
      const provider = str(c["provider"]);
      const action = str(c["actionId"]);
      return provider && action
        ? `${provider} · ${action.replaceAll("_", " ")}`
        : "No action selected";
    },
    defaults: () => ({ provider: "", actionId: "", params: {} }),
  },
];

const BY_KIND = new Map(BLOCK_KINDS.map((spec) => [spec.kind, spec]));

const SYSTEM_SPEC: Omit<BlockKindSpec, "kind" | "label"> = {
  description: "Product logic shipped with a built-in automation.",
  icon: Lock,
  inspector: "system",
  system: true,
  summary: () => "Set by the built-in",
  defaults: () => ({}),
};

/** Resolve a block kind. `system.*` kinds are synthesized read-only. */
export function blockKind(kind: string): BlockKindSpec {
  const known = BY_KIND.get(kind);
  if (known) return known;
  if (kind.startsWith("system.")) {
    return {
      ...SYSTEM_SPEC,
      kind,
      label: kind
        .slice("system.".length)
        .split("_")
        .map((w) => w.charAt(0).toUpperCase() + w.slice(1))
        .join(" "),
    };
  }
  return {
    kind,
    label: kind,
    description: "Unknown block kind.",
    icon: Lock,
    inspector: "system",
    summary: () => "Unknown block kind",
    defaults: () => ({}),
  };
}

/** Kinds a user may insert (system kinds excluded). */
export function insertableBlockKinds(): readonly BlockKindSpec[] {
  return BLOCK_KINDS;
}

// ---------------------------------------------------------------------------
// Dotted-path helpers for generic fields ("waitFor.kind")
// ---------------------------------------------------------------------------

export function getPath(config: BlockConfig, path: string): unknown {
  let current: unknown = config;
  for (const segment of path.split(".")) {
    if (typeof current !== "object" || current === null) return undefined;
    current = (current as Record<string, unknown>)[segment];
  }
  return current;
}

export function setPath(config: BlockConfig, path: string, value: unknown): BlockConfig {
  const segments = path.split(".");
  const next: BlockConfig = { ...config };
  let cursor: Record<string, unknown> = next;
  for (const segment of segments.slice(0, -1)) {
    const child = cursor[segment];
    const copy =
      typeof child === "object" && child !== null && !Array.isArray(child)
        ? { ...(child as Record<string, unknown>) }
        : {};
    cursor[segment] = copy;
    cursor = copy;
  }
  const last = segments[segments.length - 1]!;
  if (value === undefined || value === "") delete cursor[last];
  else cursor[last] = value;
  return next;
}

/** The top-level config key a dotted field belongs to (tunable lists name
 * top-level keys). */
export function topLevelKey(path: string): string {
  return path.split(".")[0]!;
}

// ---------------------------------------------------------------------------
// Tree walking
// ---------------------------------------------------------------------------

export function* walkBlocks(blocks: readonly BlockDef[]): Generator<BlockDef> {
  for (const block of blocks) {
    yield block;
    if (block.then) yield* walkBlocks(block.then);
    if (block.else) yield* walkBlocks(block.else);
    if (block.body) yield* walkBlocks(block.body);
  }
}

export function findBlock(blocks: readonly BlockDef[], id: string): BlockDef | undefined {
  for (const block of walkBlocks(blocks)) if (block.id === id) return block;
  return undefined;
}

/** Return a new tree with `id` replaced by `next` (identity-preserving elsewhere). */
export function replaceBlock(blocks: readonly BlockDef[], id: string, next: BlockDef): BlockDef[] {
  return blocks.map((block) => {
    if (block.id === id) return next;
    const out: BlockDef = { ...block };
    if (block.then) out.then = replaceBlock(block.then, id, next);
    if (block.else) out.else = replaceBlock(block.else, id, next);
    if (block.body) out.body = replaceBlock(block.body, id, next);
    return out;
  });
}

export function removeBlock(blocks: readonly BlockDef[], id: string): BlockDef[] {
  return blocks
    .filter((block) => block.id !== id)
    .map((block) => {
      const out: BlockDef = { ...block };
      if (block.then) out.then = removeBlock(block.then, id);
      if (block.else) out.else = removeBlock(block.else, id);
      if (block.body) out.body = removeBlock(block.body, id);
      return out;
    });
}

/** Where a child list lives: the root, or one of a nesting block's slots. */
export type ListPath = { root: true } | { parentId: string; slot: "then" | "else" | "body" };

function listAt(blocks: readonly BlockDef[], at: ListPath): BlockDef[] | undefined {
  if ("root" in at) return [...blocks];
  const parent = findBlock(blocks, at.parentId);
  return parent ? [...(parent[at.slot] ?? [])] : undefined;
}

function withList(blocks: readonly BlockDef[], at: ListPath, list: BlockDef[]): BlockDef[] {
  if ("root" in at) return list;
  const parent = findBlock(blocks, at.parentId);
  if (!parent) return [...blocks];
  return replaceBlock(blocks, at.parentId, { ...parent, [at.slot]: list });
}

export function insertBlock(
  blocks: readonly BlockDef[],
  at: ListPath,
  index: number,
  block: BlockDef,
): BlockDef[] {
  const list = listAt(blocks, at);
  if (!list) return [...blocks];
  list.splice(Math.max(0, Math.min(index, list.length)), 0, block);
  return withList(blocks, at, list);
}

/** Move a block within one list (drag reorder). */
export function moveBlock(
  blocks: readonly BlockDef[],
  at: ListPath,
  from: number,
  to: number,
): BlockDef[] {
  const list = listAt(blocks, at);
  if (!list || from < 0 || from >= list.length) return [...blocks];
  const [moved] = list.splice(from, 1);
  list.splice(Math.max(0, Math.min(to, list.length)), 0, moved!);
  return withList(blocks, at, list);
}

/** A unique, id-safe block id for a new block of `kind`. */
export function nextBlockId(blocks: readonly BlockDef[], kind: string): string {
  const base = kind.replace(/^system\./, "").replace(/[^a-z0-9_]/g, "_");
  const taken = new Set([...walkBlocks(blocks)].map((b) => b.id));
  if (!taken.has(base)) return base;
  let n = 2;
  while (taken.has(`${base}_${n}`)) n += 1;
  return `${base}_${n}`;
}

// ---------------------------------------------------------------------------
// Built-in editing model: structure locked, tunable fields editable
// ---------------------------------------------------------------------------

export function isTunable(block: BlockDef, fieldPath: string): boolean {
  return (block.tunable ?? []).includes(topLevelKey(fieldPath));
}

export type BlockOverrides = Record<string, Record<string, unknown>>;

/** Apply overrides over a definition's block configs (what the engine does at
 * snapshot time), so the editor shows the effective config. */
export function applyOverrides(
  definition: AutomationDefinition,
  overrides: BlockOverrides,
): AutomationDefinition {
  const apply = (blocks: readonly BlockDef[]): BlockDef[] =>
    blocks.map((block) => {
      const override = overrides[block.id];
      const out: BlockDef = override
        ? { ...block, config: { ...block.config, ...override } }
        : { ...block };
      if (block.then) out.then = apply(block.then);
      if (block.else) out.else = apply(block.else);
      if (block.body) out.body = apply(block.body);
      return out;
    });
  return { ...definition, blocks: apply(definition.blocks) };
}

/** The overrides a built-in edit produces: for each block, only the tunable
 * top-level keys whose effective value differs from the shipped version.
 *
 * An override is a VALUE layered over the shipped config (the server merges
 * with spread and re-validates); there is no "unset" representation, and a
 * literal null fails every `.optional()` string schema. So a field the user
 * CLEARS (absent in the edit) is treated as "revert to shipped" — the key is
 * omitted and the shipped value wins. Under the editing model that is the
 * honest reading: a built-in's config is tuned, never made smaller than
 * shipped. Expressing "unset a shipped value" would need a server-side
 * sentinel; until then the Harness control labels the cleared state
 * "Shipped default". */
export function diffOverrides(
  shipped: AutomationDefinition,
  edited: AutomationDefinition,
): BlockOverrides {
  const out: BlockOverrides = {};
  const shippedById = new Map([...walkBlocks(shipped.blocks)].map((b) => [b.id, b]));
  for (const block of walkBlocks(edited.blocks)) {
    const base = shippedById.get(block.id);
    if (!base) continue;
    for (const key of base.tunable ?? []) {
      const after = block.config[key];
      if (after === undefined) continue; // cleared → revert to shipped
      if (JSON.stringify(base.config[key]) !== JSON.stringify(after)) {
        (out[block.id] ??= {})[key] = after;
      }
    }
  }
  return out;
}

// ---------------------------------------------------------------------------
// BlockError routing
// ---------------------------------------------------------------------------

export interface BlockErrorRef {
  blockId: string;
  field: string;
  message: string;
}

/** The server joins BlockErrors into the ConnectError message as
 * `blockId.field: message; …` (definition-level: `field: message`). Parse
 * that back into addressable errors; anything unparseable is a form error. */
export function parseBlockErrors(message: string): BlockErrorRef[] {
  const out: BlockErrorRef[] = [];
  for (const part of message.split("; ")) {
    const m = /^([a-z][a-z0-9_]*)\.([A-Za-z0-9_.[\]]+): (.*)$/.exec(part);
    if (m) {
      // `trigger.<field>` is definition-level: the trigger row owns it.
      if (m[1] === "trigger") out.push({ blockId: "", field: `trigger.${m[2]!}`, message: m[3]! });
      else out.push({ blockId: m[1]!, field: m[2]!, message: m[3]! });
      continue;
    }
    const d = /^([A-Za-z0-9_.[\]]+): (.*)$/.exec(part);
    if (d) out.push({ blockId: "", field: d[1]!, message: d[2]! });
    else out.push({ blockId: "", field: "form", message: part });
  }
  return out;
}

export const EMPTY_DEFINITION: AutomationDefinition = {
  engine: 1,
  trigger: { kind: "manual" },
  blocks: [],
  inputsSchema: [],
  settings: { endSessionsOnFinish: false },
};

export function parseDefinition(json: string | undefined): AutomationDefinition {
  if (!json) return EMPTY_DEFINITION;
  try {
    const parsed: unknown = JSON.parse(json);
    if (
      typeof parsed === "object" &&
      parsed !== null &&
      Array.isArray((parsed as { blocks?: unknown }).blocks)
    ) {
      return parsed as AutomationDefinition;
    }
  } catch {
    // fall through
  }
  return EMPTY_DEFINITION;
}

export function parseOverrides(json: string | undefined): BlockOverrides {
  if (!json) return {};
  try {
    const parsed: unknown = JSON.parse(json);
    return typeof parsed === "object" && parsed !== null ? (parsed as BlockOverrides) : {};
  } catch {
    return {};
  }
}
