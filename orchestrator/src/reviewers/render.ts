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

/** Guest directory where the review workflow stages rendered files. */
export const REVIEW_GUEST_DIR = "/workspace/.review";

export interface RenderReviewerOptions {
  role: ReviewerRole;
  /** Defaults to all six, in REVIEW_CATEGORIES order. */
  enabledCategories?: readonly ReviewCategory[];
  /** Free text from org settings; empty/undefined renders a fixed placeholder. */
  orgInstructions?: string;
}

/** A file to stage into the guest through the streaming file writer. */
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
const SLOT_PATTERN_GLOBAL = /\{\{[^{}]+\}\}/g;

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
  "- `body_md` (string, required) — WHAT / WHEN: what the problem is, in one",
  "  sentence, and when the issue can be hit, explained clearly. WHEN ends with",
  "  `Trigger likelihood: <routine|plausible-fault|compound-fault|operator-misuse>`.",
  "- `suggested_fix` (string, optional) — a concrete fix only when it is clear",
  "  from code you read. For a structural finding, sketch the structural fix —",
  "  never a per-instance point patch.",
  "- `evidence` (string[], required) — the files you actually read to reach this",
  "  finding. A finding about a file not in this list is invalid; a subagent's",
  "  report is a lead, not evidence.",
  "",
  "Severity and confidence are different questions. Severity is the impact",
  "under the plausible trigger you named — never under a rarer one — and",
  "confidence is how sure you are the finding is real. Never average the two",
  "into `medium`.",
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
  "  traced the failure yourself, the trigger is plausible (or the repo demands",
  "  that paranoia), and the category lens does not exclude it; `refuted` when",
  "  the code disproves it, or it fails the staleness, duplicate, reachability,",
  "  or lens-bar gate.",
  "- `confidence` (`\"high\"` | `\"medium\"` | `\"low\"`, required) — how sure you are of",
  "  your verdict.",
  "- `reasoning` (string, required) — the specific code evidence (file and",
  "  behavior) behind the verdict. A gate refutation starts with its tag —",
  "  `stale:`, `duplicate:`, or `below-bar:` — and its justification. A verdict",
  "  argued only from the finding's own text is invalid, except `duplicate:`,",
  "  which cites the prior finding.",
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

  // Fill every slot in a SINGLE pass with a function replacement. Two properties
  // matter: (1) function replacements are always literal, so a filled value
  // containing `$&`/`$1`/`$$` is inserted verbatim (a string replacement would
  // interpret them and could even leave the slot behind), and (2) one pass means
  // inserted text is never rescanned, so a value that happens to contain another
  // slot's text (e.g. org instructions mentioning `{{TOOL_CONTRACT}}`) can't be
  // re-expanded into an injection.
  const slotValues = new Map<string, string>([
    ["{{ORG_INSTRUCTIONS}}", orgInstructions],
    ["{{TOOL_CONTRACT}}", toolContract],
  ]);
  if (opts.role === "finder") {
    slotValues.set("{{ENABLED_CATEGORIES}}", categoryList(categories));
  }

  let unfilledSlot: string | undefined;
  const roleContent = loadMarkdown(`${opts.role}.md`).replace(SLOT_PATTERN_GLOBAL, (slot) => {
    const value = slotValues.get(slot);
    if (value === undefined) {
      unfilledSlot ??= slot;
      return slot;
    }
    return value;
  });
  if (unfilledSlot !== undefined) {
    throw new Error(`unfilled reviewer slot: ${unfilledSlot}`);
  }

  const files: RenderedReviewerFile[] = [
    {
      path: join(REVIEW_GUEST_DIR, `${opts.role}.md`),
      content: roleContent,
    },
  ];

  // Both roles get the lens files: the finder hunts with them, and the
  // verifier enforces each lens's "Do not report" bar on the candidates.
  for (const category of categories) {
    files.push({
      path: join(REVIEW_GUEST_DIR, "lenses", `${category}.md`),
      content: loadMarkdown(join("lenses", `${category}.md`)),
    });
  }

  return files;
}
