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
  Database,
  Eraser,
  Filter,
  FilePlus2,
  GitBranch,
  GitPullRequest,
  HeartPulse,
  Hourglass,
  Link2,
  List,
  Lock,
  SquareCheck,
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

export interface AutomationEntrypoint {
  id: string;
  trigger: TriggerSpec;
  blocks: BlockDef[];
}

export interface AutomationDefinition {
  engine: number;
  trigger: TriggerSpec;
  blocks: BlockDef[];
  /** ADR 0119 D9: additional named ways in. The top-level trigger + blocks
   * are the implicit "main" entrypoint. */
  entrypoints?: AutomationEntrypoint[];
  inputsSchema: unknown[];
  settings: {
    concurrency?: {
      keyTemplate: string;
      policy: "queue" | "supersede" | "skip" | "join";
    };
    runDeadlineSeconds?: number;
    endSessionsOnFinish: boolean;
    /** ADR 0120: present = the automation has workstreams. */
    instance?: {
      keyTemplate: string;
      inputs?: Record<string, string>;
      entrypoints?: Record<string, { admit: "open" | "require" | "handle_match" }>;
    };
  };
}

// ---------------------------------------------------------------------------
// Entrypoints (ADR 0119 D9): the Build tab edits ONE entrypoint at a time
// through a projection — the projected definition carries the selected
// entrypoint's trigger + blocks at the top level, so BuildTab and every tree
// helper stay entrypoint-blind.
// ---------------------------------------------------------------------------

export const MAIN_ENTRYPOINT_ID = "main";
export const ENTRYPOINT_ID_RE = /^[a-z][a-z0-9_]*$/;

export function entrypointIds(definition: AutomationDefinition): string[] {
  return [MAIN_ENTRYPOINT_ID, ...(definition.entrypoints ?? []).map((ep) => ep.id)];
}

/** The definition as seen from one entrypoint: its trigger + blocks at the
 * top level. `main` is the definition itself. */
export function projectEntrypoint(
  definition: AutomationDefinition,
  entrypointId: string,
): AutomationDefinition {
  if (entrypointId === MAIN_ENTRYPOINT_ID) return definition;
  const ep = definition.entrypoints?.find((e) => e.id === entrypointId);
  if (!ep) return definition;
  return { ...definition, trigger: ep.trigger, blocks: ep.blocks };
}

/** Fold an edited projection back: the projected trigger + blocks land in
 * the entrypoint, everything else (settings, inputsSchema, …) lands on the
 * definition, and main's own trigger + blocks are restored. */
export function mergeEntrypoint(
  definition: AutomationDefinition,
  entrypointId: string,
  next: AutomationDefinition,
): AutomationDefinition {
  if (entrypointId === MAIN_ENTRYPOINT_ID) return next;
  return {
    ...next,
    trigger: definition.trigger,
    blocks: definition.blocks,
    entrypoints: (definition.entrypoints ?? []).map((ep) =>
      ep.id === entrypointId ? { ...ep, trigger: next.trigger, blocks: next.blocks } : ep,
    ),
  };
}

/** null = ok; otherwise the reason the id is unusable. */
export function entrypointIdError(definition: AutomationDefinition, id: string): string | null {
  if (!ENTRYPOINT_ID_RE.test(id))
    return "Lowercase letters, digits, underscores; starts with a letter.";
  if (id === MAIN_ENTRYPOINT_ID) return '"main" names the implicit top-level entrypoint.';
  if (entrypointIds(definition).includes(id)) return `"${id}" already exists.`;
  return null;
}

export function addEntrypoint(definition: AutomationDefinition, id: string): AutomationDefinition {
  return {
    ...definition,
    entrypoints: [
      ...(definition.entrypoints ?? []),
      { id, trigger: { kind: "manual" }, blocks: [] },
    ],
  };
}

export function removeEntrypoint(
  definition: AutomationDefinition,
  id: string,
): AutomationDefinition {
  const rest = (definition.entrypoints ?? []).filter((ep) => ep.id !== id);
  const { entrypoints: _dropped, ...base } = definition;
  return rest.length > 0 ? { ...base, entrypoints: rest } : base;
}

/** Block ids OUTSIDE one entrypoint — the reserved set for nextBlockId
 * (server validation holds ids unique across ALL entrypoints). */
export function blockIdsOutsideEntrypoint(
  definition: AutomationDefinition,
  entrypointId: string,
): string[] {
  const ids: string[] = [];
  const collect = (blocks: readonly BlockDef[]) => {
    for (const block of walkBlocks(blocks)) ids.push(block.id);
  };
  if (entrypointId !== MAIN_ENTRYPOINT_ID) collect(definition.blocks);
  for (const ep of definition.entrypoints ?? []) {
    if (ep.id !== entrypointId) collect(ep.blocks);
  }
  return ids;
}

export type FieldSpec =
  | {
      type: "template";
      key: string;
      label: string;
      multiline?: boolean;
      help?: string;
    }
  | { type: "string"; key: string; label: string; help?: string }
  | {
      type: "number";
      key: string;
      label: string;
      help?: string;
      min?: number;
      max?: number;
    }
  | { type: "boolean"; key: string; label: string; help?: string }
  | { type: "duration"; key: string; label: string; help?: string }
  | { type: "session_ref"; key: string; label: string; help?: string }
  | {
      type: "select";
      key: string;
      label: string;
      options: readonly string[];
      help?: string;
    }
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
    | "readonly";
  /** Whether this kind nests child lists (branch: then/else; loop: body). */
  nests?: "branch" | "loop";
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
    defaults: () => ({
      profileId: "",
      promptTemplate: "",
      includeEventContext: false,
    }),
  },
  {
    kind: "send_prompt",
    label: "Send prompt",
    description: "Send a prompt to a session and optionally wait for it to finish.",
    icon: Send,
    fields: [
      { type: "session_ref", key: "session", label: "Session" },
      {
        type: "template",
        key: "promptTemplate",
        label: "Prompt",
        multiline: true,
      },
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
      {
        type: "string",
        key: "harnessMode",
        label: "Harness mode",
        help: "e.g. plan (optional)",
      },
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
      {
        type: "select",
        key: "until",
        label: "Until",
        options: ["idle", "ended"],
      },
      WAIT_DEADLINE_FIELD,
    ],
    summary: (c) => `${sessionRefLabel(c["session"])} · until ${str(c["until"], "idle")}`,
    defaults: () => ({ session: { blockId: "" }, until: "idle" }),
  },
  {
    kind: "session_status",
    label: "Session status",
    description: "Probe a session read-only: status, idle time, and whether a live run owns it.",
    icon: HeartPulse,
    fields: [{ type: "session_ref", key: "session", label: "Session" }],
    summary: (c) => `probe ${sessionRefLabel(c["session"])}`,
    defaults: () => ({ session: { template: "" } }),
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
      {
        type: "template",
        key: "commandTemplate",
        label: "Command",
        multiline: true,
      },
      {
        type: "number",
        key: "deadlineMs",
        label: "Deadline (ms)",
        min: 1000,
        max: 600000,
      },
      {
        type: "boolean",
        key: "allowNonZeroExit",
        label: "Allow non-zero exit",
      },
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
    kind: "state_get",
    label: "Read state",
    description: "Read one entry from this automation's shared state.",
    icon: Database,
    fields: [
      {
        type: "template",
        key: "key",
        label: "Key",
        help: "One document per entity, e.g. ticket:${{ event.raw.id }}",
      },
    ],
    summary: (c) => truncate(str(c["key"]), 48) || "No key",
    defaults: () => ({ key: "" }),
  },
  {
    kind: "state_set",
    label: "Write state",
    description: "Write one entry; an expected version turns the write into a compare-and-swap.",
    icon: Database,
    fields: [
      {
        type: "template",
        key: "key",
        label: "Key",
        help: "One document per entity, e.g. ticket:${{ event.raw.id }}",
      },
      {
        type: "json",
        key: "value",
        label: "Value",
        help: 'JSON, or a run-time reference like {"$ref": "steps.facts.value"}.',
      },
      {
        type: "number",
        key: "expectVersion",
        label: "Expect version",
        min: 0,
        help: "Optional CAS: 0 = create only. A miss is steps.<id>.ok = false, never an error.",
      },
    ],
    summary: (c) => truncate(str(c["key"]), 48) || "No key",
    defaults: () => ({ key: "", value: {} }),
  },
  {
    kind: "state_delete",
    label: "Delete state",
    description: "Delete one entry (idempotent); an expected version makes it conditional.",
    icon: Eraser,
    fields: [
      { type: "template", key: "key", label: "Key" },
      { type: "number", key: "expectVersion", label: "Expect version", min: 0 },
    ],
    summary: (c) => truncate(str(c["key"]), 48) || "No key",
    defaults: () => ({ key: "" }),
  },
  {
    kind: "state_list",
    label: "List state",
    description: "List entries by key prefix (up to 500), oldest key first.",
    icon: List,
    fields: [
      {
        type: "template",
        key: "prefix",
        label: "Key prefix",
        help: "Empty = every entry.",
      },
      { type: "number", key: "limit", label: "Limit", min: 1, max: 500 },
    ],
    summary: (c) => {
      const prefix = str(c["prefix"]);
      return prefix ? `prefix ${truncate(prefix, 40)}` : "every entry";
    },
    defaults: () => ({ prefix: "" }),
  },
  {
    kind: "lookup_pr_session",
    label: "Look up PR session",
    description: "Map a pull request to the session that authored it (the pr_ref ledger).",
    icon: GitPullRequest,
    fields: [
      {
        type: "template",
        key: "repo",
        label: "Repository",
        help: "owner/name, e.g. ${{ event.raw.repository.full_name }}",
      },
      {
        type: "number",
        key: "prNumber",
        label: "PR number",
        min: 1,
        help: 'Usually a run-time reference: {"$ref": "event.raw.pull_request.number"}.',
      },
    ],
    summary: (c) => {
      const repo = str(c["repo"]);
      return repo ? `PR in ${truncate(repo, 40)}` : "No repository";
    },
    defaults: () => ({ repo: "", prNumber: 1 }),
  },
  {
    kind: "instance_close",
    label: "Close workstream",
    description: "Close the run's own workstream; later events for it are dropped (audited).",
    icon: SquareCheck,
    fields: [
      {
        type: "template",
        key: "reason",
        label: "Reason",
        help: "Kept on the workstream for the audit trail.",
      },
    ],
    summary: (c) => {
      const reason = str(c["reason"]);
      return reason ? truncate(reason, 40) : "Close this workstream";
    },
    defaults: () => ({}),
  },
  {
    kind: "claim_handle",
    label: "Claim handle",
    description:
      "Route an external identifier to this workstream (a channel, a ticket id). Posts and PRs bind automatically.",
    icon: Link2,
    fields: [
      {
        type: "template",
        key: "handle",
        label: "Handle",
        help: "Full handle with its namespace prefix, e.g. slack:${{ inputs.channel_id }}",
      },
    ],
    summary: (c) => {
      const handle = str(c["handle"]);
      return handle ? truncate(handle, 40) : "No handle";
    },
    defaults: () => ({ handle: "" }),
  },
  {
    kind: "review_open_pass",
    label: "Open review pass",
    description:
      "Start a review pass on a pull request in the engrams review ledger (it shows on the Reviews page). Resolves the heads from GitHub when the trigger carries no SHAs.",
    icon: GitPullRequest,
    fields: [
      {
        type: "template",
        key: "repo",
        label: "Repository",
        help: "owner/name, e.g. ${{ event.raw.repository.full_name }}",
      },
      {
        type: "number",
        key: "prNumber",
        label: "PR number",
        min: 1,
        help: 'Usually a run-time reference: {"$ref": "event.raw.pull_request.number"}.',
      },
      {
        type: "select",
        key: "trigger",
        label: "Trigger",
        options: ["opened", "synchronize", "ready_for_review", "command", "retry", "dispatch"],
        help: "opened and synchronize deduplicate a repeat on the same head; the others are requests.",
      },
      {
        type: "template",
        key: "headSha",
        label: "Head SHA",
        help: "Leave empty to resolve from GitHub.",
      },
      {
        type: "template",
        key: "baseSha",
        label: "Base SHA",
        help: "Leave empty to resolve from GitHub.",
      },
      {
        type: "json",
        key: "pr",
        label: "PR context",
        help: "Optional facts from the webhook payload (title, author, state, url, …).",
      },
    ],
    summary: (c) => {
      const repo = str(c["repo"]);
      const trigger = str(c["trigger"], "opened");
      return repo ? `${trigger} · ${truncate(repo, 40)}` : "No repository";
    },
    defaults: () => ({ provider: "github", repo: "", prNumber: 1, trigger: "opened" }),
  },
  {
    kind: "review_stage",
    label: "Stage review worker",
    description:
      "Write the reviewer brief, prior findings, and candidates into a session for one phase, and compose that phase's prompt (the prompt output feeds send_prompt).",
    icon: FilePlus2,
    fields: [
      { type: "select", key: "phase", label: "Phase", options: ["finder", "verifier"] },
      {
        type: "template",
        key: "reviewId",
        label: "Review id",
        help: "From the open pass: ${{ steps.open.review_id }}",
      },
      {
        type: "template",
        key: "sessionId",
        label: "Session id",
        help: "The worker session: ${{ steps.finder.session_id }}",
      },
      { type: "template", key: "repo", label: "Repository" },
      { type: "number", key: "prNumber", label: "PR number", min: 1 },
      { type: "template", key: "headSha", label: "Head SHA" },
      { type: "template", key: "baseSha", label: "Base SHA" },
      {
        type: "json",
        key: "enabledCategories",
        label: "Categories",
        help: "The lenses the reviewer applies; empty = all.",
      },
      { type: "template", key: "orgInstructions", label: "Org instructions", multiline: true },
      {
        type: "template",
        key: "focus",
        label: "Focus",
        multiline: true,
        help: "Finder only: a directive appended to the prompt.",
      },
    ],
    summary: (c) => `Stage the ${str(c["phase"], "finder")}`,
    defaults: () => ({
      phase: "finder",
      reviewId: "",
      sessionId: "",
      repo: "",
      prNumber: 1,
      headSha: "",
    }),
  },
  {
    kind: "review_settle",
    label: "Settle review",
    description:
      "Apply the category and severity policy to the pass's findings, settle them in the ledger, and emit the parameters for github.post_pr_review.",
    icon: SquareCheck,
    fields: [
      {
        type: "template",
        key: "reviewId",
        label: "Review id",
        help: "${{ steps.open.review_id }}",
      },
      {
        type: "template",
        key: "sessionId",
        label: "Verifier session id",
        help: "Optional: retired first, best effort.",
      },
    ],
    summary: () => "Decide and settle the findings",
    defaults: () => ({ reviewId: "" }),
  },
  {
    kind: "review_close_pass",
    label: "Close review pass",
    description:
      "Mark a pass failed or halted (sticky status comment + activity event) or tear a superseded pass down. Meant for settings.onFinalize hooks.",
    icon: CircleStop,
    fields: [
      {
        type: "template",
        key: "reviewId",
        label: "Review id",
        help: "${{ steps.open.review_id }}",
      },
      {
        type: "select",
        key: "outcome",
        label: "Outcome",
        options: ["failed", "halted", "superseded"],
      },
      {
        type: "template",
        key: "reason",
        label: "Reason",
        multiline: true,
        help: "Recorded on the review's activity log (failed only), e.g. ${{ run.error }}.",
      },
    ],
    summary: (c) => `Close as ${str(c["outcome"], "failed")}`,
    defaults: () => ({ reviewId: "", outcome: "failed" }),
  },
  {
    kind: "resolve_user",
    label: "Resolve user",
    description:
      "Map a provider identity (a Slack user id) to an engrams user. Outputs found + user_id; pass user_id to create_session as the owner.",
    icon: HeartPulse,
    fields: [
      { type: "select", key: "provider", label: "Provider", options: ["slack"] },
      {
        type: "template",
        key: "externalUserId",
        label: "External user id",
        help: "e.g. ${{ event.raw.event.user }}",
      },
    ],
    summary: (c) => {
      const id = str(c["externalUserId"]);
      return id ? `${str(c["provider"], "slack")} user ${truncate(id, 30)}` : "No user id";
    },
    defaults: () => ({ provider: "slack", externalUserId: "" }),
  },
  {
    kind: "relay_session",
    label: "Relay session",
    description:
      "Mirror a session into a Slack thread: streamed replies as bubbles, questions as Block Kit, ⏳/✅ on the message that started the turn. Run it again to re-point at a follow-up.",
    icon: MailOpen,
    fields: [
      { type: "session_ref", key: "session", label: "Session" },
      { type: "select", key: "provider", label: "Provider", options: ["slack"] },
      { type: "template", key: "team", label: "Team id", help: "${{ event.raw.team_id }}" },
      {
        type: "template",
        key: "channel",
        label: "Channel id",
        help: "${{ event.raw.event.channel }}",
      },
      {
        type: "template",
        key: "threadTs",
        label: "Thread ts",
        help: "The thread root: ${{ event.raw.event.thread_ts | default: event.raw.event.ts }}",
      },
      {
        type: "template",
        key: "mentionTs",
        label: "Mention ts",
        help: "The message this turn reacts on; defaults to the thread root.",
      },
      { type: "template", key: "userId", label: "Author id", help: "${{ event.raw.event.user }}" },
      { type: "template", key: "eventId", label: "Event id", help: "${{ event.raw.event_id }}" },
    ],
    summary: (c) => {
      const channel = str(c["channel"]);
      return channel ? `Relay to ${truncate(channel, 30)}` : "No channel";
    },
    defaults: () => ({ session: {}, provider: "slack", team: "", channel: "", threadTs: "" }),
  },
  {
    kind: "relay_close",
    label: "Close relay",
    description:
      "Post the closing message of a relayed session (✅ with the last reply and assets, ❌ with the error, or a neutral note). Meant for settings.onFinalize hooks.",
    icon: CircleStop,
    fields: [
      { type: "template", key: "status", label: "Run status", help: "Usually ${{ run.status }}." },
      {
        type: "template",
        key: "message",
        label: "Message",
        multiline: true,
        help: "Optional text for the failure or neutral message.",
      },
    ],
    summary: () => "Post the closing message",
    defaults: () => ({ status: "${{ run.status }}" }),
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

/** Resolve a block kind. An unknown kind (a definition saved by a newer
 * server, say) renders read-only. */
export function blockKind(kind: string): BlockKindSpec {
  const known = BY_KIND.get(kind);
  if (known) return known;
  return {
    kind,
    label: kind,
    description: "Unknown block kind.",
    icon: Lock,
    inspector: "readonly",
    summary: () => "Unknown block kind",
    defaults: () => ({}),
  };
}

/** Kinds a user may insert. */
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
export function nextBlockId(
  blocks: readonly BlockDef[],
  kind: string,
  reserved: readonly string[] = [],
): string {
  const base = kind.replace(/^system\./, "").replace(/[^a-z0-9_]/g, "_");
  const taken = new Set([...[...walkBlocks(blocks)].map((b) => b.id), ...reserved]);
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
