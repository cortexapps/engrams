/** Handle-candidate extraction (ADR 0120 instances). A leaf module: the
 * dispatchers and the action executor render facet-declared handle
 * templates through here; nothing here imports back into them.
 *
 * A handle is one concrete external identifier (`slack:<ch>:<ts>`,
 * `github:<repo>#<n>`) — exact-value routing keys, deliberately NOT an
 * expression language (the Message-ID precedent). A template whose path
 * resolves to nothing yields no candidate: a top-level Slack message has no
 * thread_ts, so it simply produces no thread handle.
 */

import type { HandleTemplatePart, WebhookFacet } from "../connectors/registry.ts";
import { forgeFor } from "../forges/registry.ts";
import { ownPath } from "./paths.ts";

/** The stored form of a provider's external identifier: lower case for a
 * forge whose repository names compare without case. Handles and trigger
 * scope values both pass through here, so a write and a later match agree. */
export function canonicalIdentifier(provider: string, value: string): string {
  return forgeFor(provider)?.caseInsensitiveRepos ? value.toLowerCase() : value;
}

/** Render one template. `resolve` maps a path part to a payload value; a
 * missing / non-scalar / empty value kills the whole candidate (never a
 * partial handle). */
export function renderHandleParts(
  parts: readonly HandleTemplatePart[],
  resolve: (path: string) => unknown,
): string | null {
  let out = "";
  for (const part of parts) {
    if ("lit" in part) {
      out += part.lit;
      continue;
    }
    const value = resolve(part.path);
    if (typeof value === "string") {
      if (value.length === 0) return null;
      out += value;
    } else if (typeof value === "number" && Number.isFinite(value)) {
      out += String(value);
    } else {
      return null;
    }
  }
  return out;
}

/** All candidate handles a delivered event declares, canonicalized and
 * deduped, in declaration order. Admission looks these up in the instance
 * handle ledger. */
export function extractHandleCandidates(input: {
  provider: string;
  facet: Pick<WebhookFacet, "events"> | undefined;
  eventKey: string;
  payload: Record<string, unknown>;
}): string[] {
  const templates = input.facet?.events.find((e) => e.key === input.eventKey)?.handleCandidates;
  if (!templates || templates.length === 0) return [];
  const out: string[] = [];
  const seen = new Set<string>();
  for (const template of templates) {
    const rendered = renderHandleParts(template.parts, (path) => ownPath(input.payload, path));
    if (rendered === null) continue;
    const handle = canonicalIdentifier(input.provider, rendered);
    if (seen.has(handle)) continue;
    seen.add(handle);
    out.push(handle);
  }
  return out;
}
