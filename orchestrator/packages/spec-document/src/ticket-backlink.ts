/**
 * The §backlink every ticket carries (ADR 0114 D6, R40).
 *
 * A ticket is only trustworthy when a reader can get from it to the words it
 * came from. So every draft names a section of the *pinned* spec, and its
 * description opens with a link to that section. The link survives the trip
 * to the issue tracker, which is why it is written into the description text
 * and not held only in the row.
 *
 * The orchestrator writes the link when it stores a draft, and the canvas
 * renders the same label. One implementation keeps the two identical.
 */

/** A section of the pinned spec, resolved to what a reader sees. */
export interface SpecTicketBacklink {
  sectionId: string;
  sectionTitle: string;
  /** Where the link points: the spec tab, read-only at the pinned version. */
  href: string;
}

/** The label the canvas and the description share, for example `§Data model`. */
export function backlinkLabel(sectionTitle: string): string {
  return `§${sectionTitle}`;
}

/** The spec tab of one spec, anchored at one section. */
export function backlinkHref(specId: string, sectionId: string): string {
  return `/specs/${encodeURIComponent(specId)}?view=spec&section=${encodeURIComponent(sectionId)}`;
}

export function specTicketBacklink(
  specId: string,
  section: { id: string; title: string },
): SpecTicketBacklink {
  return {
    sectionId: section.id,
    sectionTitle: section.title,
    href: backlinkHref(specId, section.id),
  };
}

/** The first line of every description: the Markdown link to the section. */
export function backlinkLine(backlink: SpecTicketBacklink): string {
  return `[${backlinkLabel(backlink.sectionTitle)}](${backlink.href})`;
}

/**
 * Put the backlink at the top of a description, exactly once.
 *
 * The agent is asked for a backlink and often writes one itself, and a person
 * who retitles a section changes the label. Stripping any leading link before
 * writing the canonical one keeps both cases at one correct line instead of a
 * growing stack of stale ones.
 */
export function withBacklink(description: string, backlink: SpecTicketBacklink): string {
  return `${backlinkLine(backlink)}\n\n${backlinkBody(description)}`;
}

/** A description without its opening backlink line — what a person edits. */
export function backlinkBody(description: string): string {
  const lines = description.split("\n");
  while (lines.length > 0 && LEADING_LINK.test(lines[0] ?? "")) lines.shift();
  while (lines.length > 0 && (lines[0] ?? "").trim() === "") lines.shift();
  return lines.join("\n").trimEnd();
}

/** A whole line that is one Markdown link starting with the section sign. */
const LEADING_LINK = /^\s*\[\s*§[^\]]*\]\([^)]*\)\s*$/;
