/** The rules a tag's fields follow. The orchestrator is their only
 * authority: the web sends what an admin entered and shows the error. */

export const MAX_TAGS_PER_AUTOMATION = 10;
const MAX_NAME_CHARS = 32;
const MAX_DESCRIPTION_CHARS = 100;

const CONTROL = /\p{Cc}/u;
const HEX_COLOR = /^#[0-9a-f]{6}$/i;

/** Characters, not UTF-16 code units. */
function charCount(text: string): number {
  return [...text].length;
}

/** The stored form of a tag name, or null when the text is not a valid name.
 * Inner whitespace (tabs included) collapses to one space first. */
export function normalizeTagName(raw: string): string | null {
  const name = raw.trim().replace(/\s+/g, " ");
  if (name === "" || CONTROL.test(name) || charCount(name) > MAX_NAME_CHARS) return null;
  return name;
}

/** The stored form of a description ("" allowed), or null when it is too long
 * or is not one line. */
export function normalizeTagDescription(raw: string): string | null {
  const description = raw.trim();
  if (CONTROL.test(description) || charCount(description) > MAX_DESCRIPTION_CHARS) return null;
  return description;
}

/** `#rrggbb` in lowercase, or null for any other form. */
export function normalizeTagColor(raw: string): string | null {
  const color = raw.trim();
  return HEX_COLOR.test(color) ? color.toLowerCase() : null;
}
