/** The tag filter of the Automations list: the selection lives in the URL as
 * tag names (`?tags=…`), so a filtered view can be shared. */

/** The "No tags" filter value. A tag name is never empty, so it cannot match
 * a real tag. */
export const NO_TAGS = "";

/** The selected names from a route's search: one name or a list of names.
 * Other values and repeated names are dropped; the order is kept. */
export function parseTagSearch(search: Record<string, unknown>): string[] {
  const raw = search["tags"];
  const values = Array.isArray(raw) ? raw : [raw];
  return [...new Set(values.filter((value): value is string => typeof value === "string"))];
}

const sameName = (a: string, b: string) => a.toLowerCase() === b.toLowerCase();

/** Whether an automation with these tag names passes the filter: nothing
 * selected, any selected name (names are unique without regard to case), or
 * No tags for an automation without tags. */
export function matchesTagFilter(
  tagNames: readonly string[],
  selected: readonly string[],
): boolean {
  if (selected.length === 0) return true;
  if (tagNames.length === 0) return selected.includes(NO_TAGS);
  return tagNames.some((name) => selected.some((choice) => sameName(name, choice)));
}

/** The selected tags first, then the rest, each in the given order. */
export function orderTagsForDisplay<T extends { name: string }>(
  tags: readonly T[],
  selected: readonly string[],
): T[] {
  const isSelected = (tag: T) => selected.some((choice) => sameName(tag.name, choice));
  return [...tags.filter(isSelected), ...tags.filter((tag) => !isSelected(tag))];
}
