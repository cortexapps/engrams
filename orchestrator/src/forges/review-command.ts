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

/** The command on the first line that has a mention of `handle` and then a
 * verb. The mention starts the line or follows a space, and prose may come
 * before it. A `[bot]` suffix on the mention is accepted. Null when there is
 * no command, or when the handle is blank (mention commands are then off). */
export function parseReviewCommand(body: string, handle: string): EngramsCommand | null {
  const slug = normalizeMentionHandle(handle);
  if (!slug) return null;
  const command = new RegExp(
    `(?:^|\\s)@${escapeRegExp(slug)}(?:\\[bot\\])?\\s+(review|fix|stop)(?:\\s+(.*))?$`,
    "i",
  );
  let match: RegExpExecArray | null = null;
  for (const line of body.split(/\r?\n/)) {
    match = command.exec(line.trim());
    if (match) break;
  }
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
