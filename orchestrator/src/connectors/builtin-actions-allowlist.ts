/** Builtin action ids (ADR 0119 D5). A connector `actions[]` entry with
 * `execute.kind: "builtin"` must name one of these; the implementations are
 * in automations/actions/builtin.ts. The registry stays declarative — it
 * names code, never loads it. Every git forge has `<provider>.post_pr_review`.
 */

export const BUILTIN_ACTION_IDS: ReadonlySet<string> = new Set([
  "github.post_pr_review",
  "slack.post_message",
  "slack.join_channel",
  "slack.update_message",
  "slack.list_replies",
  "linear.create_issue",
]);
