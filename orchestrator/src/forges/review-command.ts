/** The commands a person gives the review app in a pull-request comment:
 * `@<handle> review [focus]`, `@<handle> fix <text>`, `@<handle> stop`. */

export type EngramsCommand =
  | { kind: "review"; focus?: string }
  | { kind: "fix"; text: string }
  | { kind: "stop" };

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/** A configured handle without its leading "@" and without a trailing
 * "[bot]": some forges name an app's login "<slug>[bot]", and people
 * mention "<slug>". */
export function normalizeMentionHandle(handle: string): string {
  return handle.trim().replace(/^@/, "").replace(/\[bot\]$/i, "").trim();
}

/** The command on the first line that starts with a mention of `handle`.
 * Lines of prose may come before it. A `[bot]` suffix on the mention is
 * accepted. Null when there is no command, or when the handle is blank
 * (mention commands are then off). */
export function parseReviewCommand(body: string, handle: string): EngramsCommand | null {
  const slug = normalizeMentionHandle(handle);
  if (!slug) return null;
  const mention = `@${escapeRegExp(slug)}(?:\\[bot\\])?`;
  const lineMatcher = new RegExp(`^${mention}(?:\\s|$)`, "i");
  const line = body
    .split(/\r?\n/)
    .map((candidate) => candidate.trim())
    .find((candidate) => lineMatcher.test(candidate));
  if (!line) return null;

  const match = new RegExp(`^${mention}\\s+(review|fix|stop)(?:\\s+([\\s\\S]*))?$`, "i").exec(line);
  if (!match) return null;
  const verb = match[1]?.toLowerCase();
  const text = sanitize(match[2] ?? "");
  if (verb === "review") {
    return text ? { kind: "review", focus: text } : { kind: "review" };
  }
  if (verb === "fix") return text ? { kind: "fix", text } : null;
  if (verb === "stop") return text ? null : { kind: "stop" };
  return null;
}

function sanitize(value: string): string {
  return value
    .replace(/[\u0000-\u001f\u007f-\u009f]/g, "")
    .trim()
    .slice(0, 500);
}
