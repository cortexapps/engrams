/** The PR-review built-in's inputs schema (ADR 0119 phase 4.3 reference
 * shape): the tab must render and validate exactly this. */
export const REVIEW_INPUTS_SCHEMA = [
  {
    key: "repos",
    label: "Repositories",
    type: "map",
    keyNoun: "repository",
    required: true,
    help: "Which repositories this review watches, and how each one triggers.",
    valueShape: {
      mode: { type: "enum", label: "Mode", values: ["auto", "on_request"], default: "on_request" },
      autofix: { type: "boolean", label: "Autofix", default: false },
    },
  },
  { key: "profile", label: "Reviewer profile", type: "string", default: "pr_reviewer" },
  { key: "mention", label: "Mention", type: "string", default: "@engrams" },
  {
    key: "categories",
    label: "Categories",
    type: "list",
    valueShape: {
      element: {
        type: "enum",
        values: [
          "functional-correctness",
          "security",
          "performance",
          "maintainability",
          "testing",
          "docs",
        ],
      },
    },
    default: ["functional-correctness", "security"],
  },
  { key: "instructions", label: "Instructions", type: "string", multiline: true },
];
