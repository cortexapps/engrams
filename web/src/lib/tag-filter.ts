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
