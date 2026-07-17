import { readFileSync } from "node:fs";
import { join } from "node:path";

export type ReviewerRole = "finder" | "verifier";

export const REVIEW_CATEGORIES = [
  "security-privacy",
  "stability-availability",
  "data-integrity-integration",
  "functional-correctness",
  "performance-scalability",
  "maintainability-quality",
] as const;
export type ReviewCategory = (typeof REVIEW_CATEGORIES)[number];

/** Guest directory the rendered files land in (WriteFiles target, later PR). */
export const REVIEW_GUEST_DIR = "/workspace/.review";

export interface RenderReviewerOptions {
  role: ReviewerRole;
  /** Defaults to all six, in REVIEW_CATEGORIES order. */
  enabledCategories?: readonly ReviewCategory[];
  /** Free text from org settings; empty/undefined renders a fixed placeholder. */
  orgInstructions?: string;
}

/** A file to stage into the guest via WriteFiles. */
export interface RenderedReviewerFile {
  /** Absolute guest path, e.g. "/workspace/.review/finder.md". */
  path: string;
  content: string;
}

const REVIEWERS_ROOT = join(import.meta.dir, "..", "..", "reviewers");
const rawMarkdownCache = new Map<string, string>();

const CATEGORY_METADATA: Record<ReviewCategory, { emoji: string; title: string }> = {
  "security-privacy": { emoji: "🔒", title: "Security & Privacy" },
  "stability-availability": { emoji: "🩺", title: "Stability & Availability" },
  "data-integrity-integration": { emoji: "🗄️", title: "Data Integrity & Integration" },
  "functional-correctness": { emoji: "🎯", title: "Functional Correctness" },
  "performance-scalability": { emoji: "🚀", title: "Performance & Scalability" },
  "maintainability-quality": { emoji: "📐", title: "Maintainability & Code Quality" },
};

const NO_ORG_INSTRUCTIONS = "_No organization instructions configured._";
const SLOT_PATTERN = /\{\{[^{}]+\}\}/;

const FINDER_TOOL_CONTRACT = [
  "You report exclusively through these tools. Prose in your transcript is never",
  "read; anything you do not submit does not exist.",
  "",
  "**`submit_finding`** — one call per issue:",
  "",
  "- `path` (string, required) — repo-relative path of the file the finding is in.",
  "- `start_line`, `end_line` (integers, optional) — the changed-line range the",
  "  finding anchors to. Omit only for a whole-file finding.",
  "- `side` (`\"LEFT\"` | `\"RIGHT\"`, optional) — diff side; `RIGHT` is the PR's new",
  "  version, `LEFT` the base. Use `RIGHT` for added/changed lines.",
  "- `category` (required) — exactly one of: `security-privacy`,",
  "  `stability-availability`, `data-integrity-integration`,",
  "  `functional-correctness`, `performance-scalability`,",
  "  `maintainability-quality`.",
  "- `severity` (required) — `critical` | `high` | `medium` | `low`. The impact",
  "  **if the finding is real**.",
  "- `confidence` (required) — `high` | `medium` | `low`. How sure you are it",
  "  **is** real.",
  "- `title` (string, required) — one-line summary.",
  "- `body_md` (string, required) — WHAT / WHY / HOW (omit HOW if a fix would be",
  "  speculative).",
  "- `suggested_fix` (string, optional) — a concrete fix only when it is clear",
  "  from code you read.",
  "- `evidence` (string[], required) — the files you actually read to reach this",
  "  finding. A finding about a file not in this list is invalid.",
  "",
  "Severity and confidence are different questions. A data-loss bug you are",
  "unsure about is `critical` severity with `low` confidence — never average",
  "them into `medium`.",
  "",
  "**`finder_done`** — call once, after every finding is submitted:",
  "",
  "- `summary_md` (string, required) — a short note on what you reviewed and how",
  "  deep you went. This ends your phase.",
].join("\n");

const VERIFIER_TOOL_CONTRACT = [
  "You report exclusively through this tool. Judge every candidate; a candidate",
  "you never submit a verdict for is treated as unverified and will not post.",
  "",
  "**`submit_verdict`** — one call per candidate finding:",
  "",
  "- `finding_id` (string, required) — the candidate's id, from the list in your",
  "  prompt / `candidates.json`.",
  "- `verdict` (`\"confirmed\"` | `\"refuted\"`, required) — `confirmed` only when you",
  "  traced the failure yourself and can name what triggers it; `refuted` when you",
  "  found the guard the finder missed, or could not reproduce the reasoning from",
  "  the code.",
  "- `confidence` (`\"high\"` | `\"medium\"` | `\"low\"`, required) — how sure you are of",
  "  your verdict.",
  "- `reasoning` (string, required) — the specific code evidence (file and",
  "  behavior) behind the verdict. A verdict argued only from the finding's own",
  "  text is invalid.",
].join("\n");

function loadMarkdown(relativePath: string): string {
  const cached = rawMarkdownCache.get(relativePath);
  if (cached !== undefined) return cached;

  const content = readFileSync(join(REVIEWERS_ROOT, relativePath), "utf8");
  rawMarkdownCache.set(relativePath, content);
  return content;
}

function orderedCategories(requested: readonly ReviewCategory[]): ReviewCategory[] {
  const selected = new Set<ReviewCategory>();
  for (const category of requested) {
    if (!REVIEW_CATEGORIES.includes(category)) {
      throw new Error(`unknown review category: ${String(category)}`);
    }
    selected.add(category);
  }
  return REVIEW_CATEGORIES.filter((category) => selected.has(category));
}

function categoryList(categories: readonly ReviewCategory[]): string {
  return categories
    .map((category) => {
      const metadata = CATEGORY_METADATA[category];
      const lensPath = join(REVIEW_GUEST_DIR, "lenses", `${category}.md`);
      return `- ${metadata.emoji} **${metadata.title}** — read \`${lensPath}\``;
    })
    .join("\n");
}

export function renderReviewer(opts: RenderReviewerOptions): RenderedReviewerFile[] {
  const categories = orderedCategories(opts.enabledCategories ?? REVIEW_CATEGORIES);
  const orgInstructions = opts.orgInstructions?.trim() || NO_ORG_INSTRUCTIONS;
  const toolContract = opts.role === "finder" ? FINDER_TOOL_CONTRACT : VERIFIER_TOOL_CONTRACT;

  let roleContent = loadMarkdown(`${opts.role}.md`);
  if (opts.role === "finder") {
    roleContent = roleContent.replaceAll("{{ENABLED_CATEGORIES}}", categoryList(categories));
  }
  roleContent = roleContent
    .replaceAll("{{ORG_INSTRUCTIONS}}", orgInstructions)
    .replaceAll("{{TOOL_CONTRACT}}", toolContract);

  const residualSlot = roleContent.match(SLOT_PATTERN)?.[0];
  if (residualSlot !== undefined) {
    throw new Error(`unfilled reviewer slot: ${residualSlot}`);
  }

  const files: RenderedReviewerFile[] = [
    {
      path: join(REVIEW_GUEST_DIR, `${opts.role}.md`),
      content: roleContent,
    },
  ];

  if (opts.role === "finder") {
    for (const category of categories) {
      files.push({
        path: join(REVIEW_GUEST_DIR, "lenses", `${category}.md`),
        content: loadMarkdown(join("lenses", `${category}.md`)),
      });
    }
  }

  return files;
}
