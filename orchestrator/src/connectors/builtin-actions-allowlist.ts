/** Builtin action ids (ADR 0119 D5). A connector `actions[]` entry with
 * `execute.kind: "builtin"` must name one of these; the table itself (the
 * TS implementations) registers in the action-executor stack item. The
 * registry stays declarative — it names code, never loads it.
 */

export const BUILTIN_ACTION_IDS: ReadonlySet<string> = new Set([
  "github.post_pr_review",
  "slack.post_message",
  "slack.join_channel",
  "slack.update_message",
  "linear.create_issue",
  "linear.create_comment",
]);
