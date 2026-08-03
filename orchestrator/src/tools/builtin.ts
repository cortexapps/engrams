import { z } from "zod";
import { ConnectError } from "@connectrpc/connect";

import {
  makeArtifactService,
  type ArtifactService,
} from "../artifacts/service.ts";
import { config } from "../config.ts";
import { sessions } from "../control-plane/client.ts";
import { makeArtifactStore } from "../db/artifacts.ts";
import type { ArtifactWithVersions } from "../db/artifacts.ts";
import { makePapercutStore, type PapercutStore } from "../db/papercuts.ts";
import { mintRawToken, rawArtifactPath } from "../crypto/raw-token.ts";
import { tools, type ToolRegistry } from "./registry.ts";

const QuestionOptionSchema = z.object({
  label: z.string(),
  description: z.string(),
});

const QuestionSchema = z.object({
  question: z.string(),
  header: z.string(),
  multiSelect: z.boolean(),
  options: z.array(QuestionOptionSchema),
});

export const QuestionsSchema = z.object({
  questions: z.array(QuestionSchema),
});

export const AnswersSchema = z.record(z.string(), z.array(z.string()));

export const PlanSchema = z.object({
  plan: z.string().describe("The complete implementation plan, as markdown"),
});

export const PlanDecisionSchema = z.object({
  decision: z.enum(["approve", "reject"]),
  feedback: z.string().optional(),
});

export interface BuiltinToolDeps {
  papercuts?: PapercutStore;
  /** The shared artifact service layer (defaults to the real store +
   * coordinator pull client). */
  artifacts?: ArtifactService;
  now?: () => Date;
}

// ---------------------------------------------------------------------------
// The Artifact tool (single tool, action-discriminated — modeled on
// Claude Code's Artifact tool)
// ---------------------------------------------------------------------------

const ArtifactActionSchema = z.discriminatedUnion("action", [
  z.object({
    action: z.literal("publish"),
    file_path: z.string().describe("Path of the HTML or Markdown file in this session"),
    title: z.string().optional().describe("Display title; defaults to the file name"),
  }),
  z.object({
    action: z.literal("update"),
    artifact_id: z.string(),
    file_path: z.string().describe("Path of the new version's file in this session"),
    title: z.string().optional(),
  }),
  z.object({
    action: z.literal("list"),
    scope: z
      .enum(["mine", "shared"])
      .optional()
      .describe('"mine" (default) or "shared" (artifacts shared with the org)'),
  }),
  z.object({ action: z.literal("get"), artifact_id: z.string() }),
  z.object({ action: z.literal("share"), artifact_id: z.string() }),
  z.object({ action: z.literal("unshare"), artifact_id: z.string() }),
]);

const ToolArtifactSchema = z.object({
  id: z.string(),
  title: z.string(),
  file_name: z.string(),
  media_type: z.string(),
  size_bytes: z.number(),
  visibility: z.string(),
  current_version: z.number(),
  url: z.string().describe("Stable page URL (share this)"),
  raw_url: z.string().describe("Short-lived direct byte URL (fetch this)"),
  created_at: z.string(),
  updated_at: z.string(),
});

const ArtifactOutputSchema = z.object({
  artifact: ToolArtifactSchema.optional(),
  artifacts: z.array(ToolArtifactSchema).optional(),
  total_count: z.number().optional(),
});

function defaultArtifactService(): ArtifactService {
  return makeArtifactService({
    store: makeArtifactStore(),
    pull: sessions,
  });
}

function toolArtifact(
  row: {
    id: string;
    title: string;
    fileName: string;
    mediaType: string;
    sizeBytes: number;
    visibility: string;
    currentVersion: number;
    createdAt: Date;
    updatedAt: Date;
  },
  now: Date,
): z.input<typeof ToolArtifactSchema> {
  const base = config.baseUrl.replace(/\/$/, "");
  return {
    id: row.id,
    title: row.title,
    file_name: row.fileName,
    media_type: row.mediaType,
    size_bytes: row.sizeBytes,
    visibility: row.visibility,
    current_version: row.currentVersion,
    url: `${base}/artifacts/${row.id}`,
    raw_url: `${base}${rawArtifactPath(row.id, mintRawToken(row.id, now))}`,
    created_at: row.createdAt.toISOString(),
    updated_at: row.updatedAt.toISOString(),
  };
}

/** Register tools that every production session receives. */
export function registerBuiltinTools(
  registry: ToolRegistry = tools,
  deps?: BuiltinToolDeps,
): void {
  // No claude binding: claude CLI >= 2.1.187 removed the AskUserQuestion
  // built-in from headless `--print` mode, so claude receives this tool
  // through the injected MCP path like any custom harness. The description
  // must carry the affordance the built-in's training used to provide.
  registry.register({
    name: "ask_user_question",
    description:
      "Ask the user one or more structured questions and wait for their " +
      "answers. Use this whenever you need a decision, clarification, or " +
      "preference from the user before you continue.",
    input: QuestionsSchema,
    output: AnswersSchema,
    handling: "session",
    execution: "deferred",
    presenters: {
      slack: "questionEffect",
      web: "UserQuestionCard",
    },
    nativeBindings: {
      codex: "requestUserInput",
    },
  });

  // ADR 0107. No native bindings: claude CLI >= 2.1.187 removed the
  // ExitPlanMode built-in from headless `--print` mode (codex never had
  // one), so every harness receives it as an injected tool through the
  // generic deferred path.
  registry.register({
    name: "exit_plan_mode",
    description:
      "Present your finished implementation plan for user approval. " +
      "Call this only in plan mode, with the complete plan as markdown. " +
      "The user approves the plan (then implement it) or rejects it with " +
      "feedback (then revise the plan).",
    input: PlanSchema,
    output: PlanDecisionSchema,
    handling: "session",
    execution: "deferred",
    presenters: {
      slack: "planEffect",
      web: "PlanCard",
    },
  });

  // The single artifact tool (owner-scoped by construction: every action
  // runs the shared service layer's CASL checks as the session's owner).
  registry.register({
    name: "Artifact",
    description:
      "Publish, update, and fetch hosted artifacts — versioned HTML or " +
      "Markdown documents with a stable URL, visible across this user's " +
      "sessions (sharing a file into the chat is separate: engram-share). " +
      "Actions: publish {file_path, title?} creates a new artifact from a " +
      "file in this session; update {artifact_id, file_path} publishes the " +
      "next version at the same URL; list {scope?} and get {artifact_id} " +
      "read the user's artifacts (raw_url is a short-lived direct byte " +
      "URL you can fetch); share/unshare {artifact_id} toggle org-wide " +
      "visibility. Only text/html and text/markdown may be published. " +
      "Author HTML artifacts as a single self-contained file (inline CSS " +
      "and JS; no external requests — they are served inside an " +
      "opaque-origin sandbox) and honor a ?theme=light|dark query " +
      "parameter so the page matches the viewer's engrams theme.",
    input: ArtifactActionSchema,
    output: ArtifactOutputSchema,
    handling: "handled",
    execution: "sync",
    handler: async (ctx, args) => {
      const service = deps?.artifacts ?? defaultArtifactService();
      const now = deps?.now ?? (() => new Date());
      if (ctx.userId === undefined) {
        return {
          error: "this session has no owning user, so it cannot manage artifacts",
        };
      }
      const actor = { id: ctx.userId, role: "user" };
      try {
        switch (args.action) {
          case "publish": {
            const row = await service.publish({
              sessionId: ctx.sessionId,
              taskId: ctx.taskId ?? null,
              ownerUserId: ctx.userId,
              filePath: args.file_path,
              ...(args.title !== undefined ? { title: args.title } : {}),
            });
            return { artifact: toolArtifact(row, now()) };
          }
          case "update": {
            const row = await service.update(actor, {
              artifactId: args.artifact_id,
              sessionId: ctx.sessionId,
              taskId: ctx.taskId ?? null,
              filePath: args.file_path,
              ...(args.title !== undefined ? { title: args.title } : {}),
            });
            return { artifact: toolArtifact(row, now()) };
          }
          case "list": {
            const { rows, totalCount } = await service.list(actor, {
              scope: args.scope ?? "mine",
              page: 1,
              pageSize: 50,
            });
            const at = now();
            return {
              artifacts: rows.map((row) => toolArtifact(row, at)),
              total_count: totalCount,
            };
          }
          case "get": {
            const row = await service.get(actor, args.artifact_id);
            return { artifact: toolArtifact(row, now()) };
          }
          case "share":
          case "unshare": {
            const row = await service.setVisibility(
              actor,
              args.artifact_id,
              args.action === "share" ? "org" : "private",
            );
            return { artifact: toolArtifact(row, now()) };
          }
        }
      } catch (error) {
        const message =
          error instanceof ConnectError
            ? error.rawMessage
            : error instanceof Error
              ? error.message
              : String(error);
        return { error: message };
      }
    },
  });

  registry.register({
    name: "papercut",
    description:
      "Log a papercut — a small, concrete friction you hit while working " +
      "(confusing error, missing or awkward tooling, docs gap, slow/flaky command, " +
      "environment quirk you had to work around). One short call, then continue your main task.",
    input: z.object({
      summary: z.string().describe("One-line summary of the friction"),
      description: z.string().describe(
        "What was painful, what you tried, and what would have helped",
      ),
      category: z.enum(["tooling", "environment", "docs", "workflow", "other"]),
      severity: z.enum(["low", "medium", "high"]).optional(),
      tags: z.array(z.string()).optional(),
    }),
    output: z.object({ logged: z.boolean(), id: z.string() }),
    handling: "handled",
    execution: "sync",
    handler: async (ctx, args) => {
      const id = await (deps?.papercuts ?? makePapercutStore()).insert({
        summary: args.summary,
        description: args.description,
        category: args.category,
        severity: args.severity ?? null,
        tags: args.tags ?? [],
        sessionId: ctx.sessionId,
        toolCallId: ctx.toolCallId,
        taskId: ctx.taskId ?? null,
        profileId: ctx.profileId ?? null,
        userId: ctx.userId ?? null,
      });
      return { logged: true, id };
    },
  });
}
