/**
 * Slack message → prompt text. Pure; no client, no network.
 *
 * A Slack message spreads its content over four independent surfaces: `text`,
 * Block Kit `blocks`, legacy `attachments` (which nest `fields` and `blocks`),
 * and `files`. Apps use them freely — the Linear app posts a one-line summary
 * as `text` and the ticket itself in an attachment.
 *
 * So this reads EVERY surface. It never rules a surface decorative; the agent,
 * not this module, judges what matters.
 *
 * It also never compares one surface against another. Two attachments that read
 * alike are two items, not a repetition — a digest that posts the same link
 * label with a different URL per item must keep every item. The ONLY content
 * dropped is a surface restating its own declared fallback, which Slack's
 * contract creates: `text` is the declared fallback for `blocks`, and an
 * attachment's `fallback` is the stand-in for that attachment's body. That
 * comparison is always between a surface and its own stand-in, never across
 * two independent pieces of content.
 */

import type { ConversationsRepliesResponse } from "@slack/web-api";

// `@slack/web-api` does not re-export its response element types, so index into
// the response instead of restating the shape — these then track the SDK.
type SdkMessage = NonNullable<ConversationsRepliesResponse["messages"]>[number];
type SdkAttachment = NonNullable<SdkMessage["attachments"]>[number];

/**
 * A reply in a Slack thread, as `conversations.replies` returns it — the SDK's
 * own type, with `blocks` widened to `unknown[]`.
 *
 * The widening is not convenience. The SDK's block types are generated from
 * sampled payloads and cannot express real Slack blocks: `BlockType` omits
 * `header` and `input`, and a `context` block's text element
 * (`{type: "mrkdwn", text: "…"}`) is unrepresentable, because `mrkdwn` is not
 * in `AccessoryType` and the element's `text` is typed as an object, never the
 * string a rich_text run carries. Widening the one broken field is honest;
 * casting real payloads into the broken type would hide the mismatch.
 */
export type SlackAttachment = Omit<SdkAttachment, "blocks"> & { blocks?: unknown[] };
export type SlackReply = Omit<SdkMessage, "blocks" | "attachments"> & {
  blocks?: unknown[];
  attachments?: SlackAttachment[];
};
type SlackFile = NonNullable<SdkMessage["files"]>[number];

const nonEmpty = (s: string | undefined): s is string => !!s && !!s.trim();

/** Render every content surface of one message, in document order. */
export function replyText(msg: SlackReply): string {
  return [
    msg.text ?? "",
    renderBlocks(msg.blocks, msg.text ?? ""),
    ...(msg.attachments ?? []).map(renderAttachment),
    ...(msg.files ?? []).map(renderFile),
  ]
    .map((part) => part.trim())
    .filter(nonEmpty)
    .join("\n");
}

/** Compare-only form, used ONLY to match a surface against its own declared
 *  fallback: unwrap Slack links, drop mrkdwn styling, fold whitespace and case,
 *  so the two renderings of the same words compare alike. */
function normalize(text: string): string {
  return text
    .replace(/<([^>|]+)\|([^>]*)>/g, "$2") // <url|label> → label
    .replace(/<([^>]+)>/g, "$1") // <url> → url
    .replace(/[*_~`]/g, "")
    .replace(/\s+/g, " ")
    .trim()
    .toLowerCase();
}

/**
 * Flatten a Block Kit tree to the strings it carries, in document order.
 *
 * This walks generically instead of switching on block type. Two reasons. The
 * SDK's block types are unusable here — a message's `blocks` and an
 * attachment's `blocks` are DIFFERENT generated types, and both omit real block
 * types (`BlockType` has no `header`) and mis-model a `context` element's text
 * (typed as an object, though a rich_text run carries a bare string). And a
 * walk reads whatever block type Slack adds later, where a switch would
 * silently skip it. Narrowing from `unknown` needs no cast, so nothing is
 * laundered.
 *
 * `covered` is Slack's plain-text stand-in for these same blocks (a message's
 * `text`, an attachment's `text`/`title`/`fallback`). Fragments already present
 * there are dropped — that is the duplication Slack's fallback contract creates.
 */
function renderBlocks(blocks: unknown, covered: string): string {
  const found: string[] = [];
  collect(blocks, found);
  const seen = normalize(covered);
  return found
    .map((f) => f.trim())
    .filter((f) => f && !seen.includes(normalize(f)))
    .join("\n");
}

/** The keys that hold content. `text` is a bare string on a rich_text run and
 *  an object on a section, so it is both emitted and descended into;
 *  `fields`/`elements`/`accessory` only nest. Every other key on a block is
 *  presentation or an opaque id. */
const CONTENT_KEYS = [
  "text",
  "title",
  "description",
  "label",
  "alt_text",
  "fields",
  "elements",
  "accessory",
  "url",
  "image_url",
] as const;

function collect(node: unknown, out: string[]): void {
  if (Array.isArray(node)) {
    for (const n of node) collect(n, out);
    return;
  }
  if (!node || typeof node !== "object") return;
  // After the object check, string-keyed access is sound; each read re-checks
  // the value's own type.
  const b = node as Record<string, unknown>;
  for (const key of CONTENT_KEYS) {
    const v = b[key];
    if (typeof v === "string") out.push(v);
    else collect(v, out);
  }
}

function renderAttachment(a: SlackAttachment): string {
  const body = [
    a.pretext,
    a.author_name,
    a.title_link && nonEmpty(a.title) ? `<${a.title_link}|${a.title}>` : a.title,
    a.text,
    ...(a.fields ?? []).map((f) => [f.title, f.value].filter(nonEmpty).join(": ")),
    renderBlocks(a.blocks, [a.title, a.text, a.fallback].filter(nonEmpty).join(" ")),
    a.footer,
  ]
    .filter(nonEmpty)
    .join("\n");
  // `fallback` is Slack's declared plain-text stand-in for the WHOLE
  // attachment, so it repeats the body — read it only when nothing else did.
  return body || (a.fallback ?? "");
}

/** The bytes stay in Slack; the prompt gets a named reference. */
function renderFile(f: SlackFile): string {
  const label = f.title?.trim() || f.name?.trim() || "file";
  const meta = [f.mimetype, f.permalink].filter(nonEmpty).join(" ");
  return meta ? `[file: ${label} ${meta}]` : `[file: ${label}]`;
}
