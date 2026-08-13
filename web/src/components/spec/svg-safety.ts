const SVG_NAMESPACE = "http://www.w3.org/2000/svg";

const ALLOWED_ELEMENTS = new Set([
  "svg",
  "g",
  "path",
  "rect",
  "circle",
  "ellipse",
  "line",
  "polyline",
  "polygon",
  "text",
  "tspan",
  "title",
  "desc",
  "defs",
  "marker",
  "lineargradient",
  "radialgradient",
  "stop",
  "clippath",
  "mask",
  "pattern",
  "symbol",
  "use",
]);

const ALLOWED_ATTRIBUTES = new Set([
  "id",
  "class",
  "role",
  "aria-label",
  "aria-hidden",
  "viewbox",
  "preserveaspectratio",
  "x",
  "y",
  "x1",
  "y1",
  "x2",
  "y2",
  "cx",
  "cy",
  "r",
  "rx",
  "ry",
  "dx",
  "dy",
  "d",
  "points",
  "width",
  "height",
  "transform",
  "pathlength",
  "opacity",
  "fill",
  "fill-opacity",
  "fill-rule",
  "stroke",
  "stroke-width",
  "stroke-opacity",
  "stroke-linecap",
  "stroke-linejoin",
  "stroke-miterlimit",
  "stroke-dasharray",
  "stroke-dashoffset",
  "paint-order",
  "color",
  "font-family",
  "font-size",
  "font-weight",
  "font-style",
  "text-anchor",
  "dominant-baseline",
  "alignment-baseline",
  "letter-spacing",
  "word-spacing",
  "vector-effect",
  "visibility",
  "display",
  "shape-rendering",
  "clip-path",
  "clip-rule",
  "mask",
  "marker-start",
  "marker-mid",
  "marker-end",
  "offset",
  "stop-color",
  "stop-opacity",
  "gradientunits",
  "gradienttransform",
  "spreadmethod",
  "fx",
  "fy",
  "fr",
  "clippathunits",
  "maskunits",
  "maskcontentunits",
  "markerwidth",
  "markerheight",
  "refx",
  "refy",
  "orient",
  "markerunits",
  "patternunits",
  "patterncontentunits",
  "patterntransform",
  "href",
  "xlink:href",
  "style",
]);

const LOCAL_REFERENCE_ATTRIBUTES = new Set([
  "clip-path",
  "mask",
  "marker-start",
  "marker-mid",
  "marker-end",
]);

const PAINT_ATTRIBUTES = new Set(["fill", "stroke", "color", "stop-color"]);

const ALLOWED_STYLE_PROPERTIES = new Set([
  "alignment-baseline",
  "clip-path",
  "clip-rule",
  "color",
  "display",
  "dominant-baseline",
  "fill",
  "fill-opacity",
  "fill-rule",
  "font-family",
  "font-size",
  "font-style",
  "font-weight",
  "letter-spacing",
  "marker-end",
  "marker-mid",
  "marker-start",
  "mask",
  "opacity",
  "paint-order",
  "shape-rendering",
  "stop-color",
  "stop-opacity",
  "stroke",
  "stroke-dasharray",
  "stroke-dashoffset",
  "stroke-linecap",
  "stroke-linejoin",
  "stroke-miterlimit",
  "stroke-opacity",
  "stroke-width",
  "text-anchor",
  "vector-effect",
  "visibility",
  "word-spacing",
]);

const LOCAL_FRAGMENT = /^#[A-Za-z0-9_.:-]+$/;
const LOCAL_URL = /^url\(\s*#[A-Za-z0-9_.:-]+\s*\)$/i;
const NETWORK_TOKEN =
  /(?:@import|expression\s*\(|(?:https?|ftp|file|blob|data|javascript|vbscript)\s*:|\/\/)/i;

/** One compound-selector charset; a backslash or angle bracket never passes. */
const SELECTOR_CHARSET = /^[\w\s.#:*>+~()[\]="'-]+$/;
const MAX_STYLESHEET_CHARS = 100_000;
const MAX_SELECTOR_CHARS = 512;

export function sanitizeSvg(svgSource: string, namespace: string): string {
  if (namespace.length === 0) throw new Error("An SVG namespace must not be empty.");
  const idPrefix = svgIdPrefix(namespace);
  const parsed = new DOMParser().parseFromString(svgSource, "image/svg+xml");
  if (
    parsed.querySelector("parsererror") ||
    parsed.documentElement.localName.toLowerCase() !== "svg" ||
    parsed.documentElement.namespaceURI !== SVG_NAMESPACE
  ) {
    throw new Error("The block renderer returned invalid SVG.");
  }

  // The scope anchor for <style> rules: an inline SVG stylesheet applies to
  // the WHOLE page, so a rule survives only when it anchors on the diagram's
  // own root id (Mermaid scopes every rule it emits this way). A root without
  // a safe id keeps no stylesheet at all.
  const rootId = parsed.documentElement.getAttribute("id");
  const styleScopeId = rootId !== null && safeIdentifier(rootId) ? rootId : null;

  const output = document.implementation.createDocument(SVG_NAMESPACE, "svg", null);
  const root = copyElement(parsed.documentElement, output, output, idPrefix, styleScopeId);
  if (!root || root !== output.documentElement) {
    throw new Error("The block renderer returned an unsafe SVG root.");
  }
  root.setAttribute("role", "presentation");
  root.removeAttribute("width");
  root.removeAttribute("height");
  return new XMLSerializer().serializeToString(root);
}

function copyElement(
  source: Element,
  parent: Element | Document,
  output: XMLDocument,
  idPrefix: string,
  styleScopeId: string | null,
): Element | null {
  const name = source.localName.toLowerCase();
  if (name === "style" && source.namespaceURI === SVG_NAMESPACE) {
    if (styleScopeId === null || parent === output) return null;
    const css = sanitizeStylesheet(source.textContent ?? "", styleScopeId, idPrefix);
    if (css === null) return null;
    const target = output.createElementNS(SVG_NAMESPACE, "style");
    target.textContent = css;
    parent.appendChild(target);
    return target;
  }
  if (source.namespaceURI !== SVG_NAMESPACE || !ALLOWED_ELEMENTS.has(name)) {
    if (name === "a" && source.namespaceURI === SVG_NAMESPACE) {
      copyChildren(source, parent, output, idPrefix, styleScopeId);
    }
    return null;
  }

  const target =
    parent === output && name === "svg"
      ? output.documentElement
      : output.createElementNS(SVG_NAMESPACE, source.localName);
  if (parent !== output) parent.appendChild(target);

  for (const attribute of source.attributes) {
    const attributeName = attribute.name.toLowerCase();
    if (!ALLOWED_ATTRIBUTES.has(attributeName)) continue;
    const value = safeAttributeValue(name, attributeName, attribute.value, idPrefix);
    if (value !== null) target.setAttribute(attribute.name, value);
  }
  copyChildren(source, target, output, idPrefix, styleScopeId);
  return target;
}

function copyChildren(
  source: Element,
  parent: Element | Document,
  output: XMLDocument,
  idPrefix: string,
  styleScopeId: string | null,
): void {
  for (const child of source.childNodes) {
    if (child.nodeType === Node.ELEMENT_NODE) {
      copyElement(child as Element, parent, output, idPrefix, styleScopeId);
    } else if (child.nodeType === Node.TEXT_NODE) {
      parent.appendChild(output.createTextNode(child.textContent ?? ""));
    }
  }
}

function safeAttributeValue(
  elementName: string,
  attributeName: string,
  rawValue: string,
  idPrefix: string,
): string | null {
  const value = rawValue.trim();
  if (value.length === 0 || containsControlCharacter(value)) return null;
  if (attributeName === "id") return safeIdentifier(value) ? namespaceId(value, idPrefix) : null;
  if (attributeName === "class") return safeClassList(value) ? value : null;
  if (attributeName === "style") return sanitizeStyle(value, idPrefix);
  if (attributeName === "href" || attributeName === "xlink:href") {
    return elementName === "use" ? namespaceLocalFragment(value, idPrefix) : null;
  }
  if (LOCAL_REFERENCE_ATTRIBUTES.has(attributeName)) {
    return value === "none" ? value : namespaceLocalUrl(value, idPrefix);
  }
  if (PAINT_ATTRIBUTES.has(attributeName)) return safePaint(value, idPrefix);
  return safeScalar(value) ? value : null;
}

/**
 * Sanitize a `<style>` element's stylesheet (Mermaid ships its theme this way).
 *
 * An inline SVG stylesheet is page-global CSS, so this is stricter than the
 * `style` attribute path in one dimension and looser in another:
 *
 * - Every selector must anchor on the diagram's own root id — an unanchored
 *   rule (`body { … }`, `.sidebar { … }`) is dropped, so a hostile cached
 *   render can never style anything outside its own block. Any `@` at-rule,
 *   backslash, comment, or stray brace drops the whole stylesheet.
 * - Declarations are filtered per-declaration against the same allowlist the
 *   `style` attribute uses (each kept declaration passes identical
 *   validation), because theme stylesheets carry harmless unlisted
 *   properties, and one of those must not cost the rule its paint.
 *
 * Returns null when nothing survives.
 */
function sanitizeStylesheet(rawCss: string, scopeId: string, idPrefix: string): string | null {
  if (rawCss.length > MAX_STYLESHEET_CHARS) return null;
  // Mermaid always emits @keyframes for its edge animations. The `animation`
  // property is not in the declaration allowlist, so the keyframes are dead
  // weight — strip the blocks (one nesting level) rather than fail the sheet.
  // Whatever remains still has to pass the flat-grammar parse below.
  const css = rawCss.replaceAll(
    /@keyframes\s+[A-Za-z0-9_-]+\s*\{(?:[^{}]*\{[^{}]*\})*[^{}]*\}/g,
    " ",
  );
  if (/[\\@<]|\/\*/.test(css) || containsControlCharacter(css.replaceAll(/[\r\n\t]/g, " "))) {
    return null;
  }
  const anchor = new RegExp(`^#${escapeRegExp(scopeId)}(?=$|[\\s.#:[>+~])`);
  const rules: string[] = [];
  const rulePattern = /([^{}]*)\{([^{}]*)\}/g;
  let consumedUpTo = 0;
  for (const match of css.matchAll(rulePattern)) {
    // Text between rules must be blank — a stray brace means a structure this
    // parser does not understand, and guessing is not sanitizing.
    if (css.slice(consumedUpTo, match.index).trim().length > 0) return null;
    consumedUpTo = match.index + match[0].length;
    const selectors: string[] = [];
    for (const rawSelector of match[1]!.split(",")) {
      const selector = rawSelector.trim();
      if (
        selector.length === 0 ||
        selector.length > MAX_SELECTOR_CHARS ||
        !SELECTOR_CHARSET.test(selector) ||
        !anchor.test(selector)
      ) {
        selectors.length = 0;
        break;
      }
      selectors.push(
        selector.replaceAll(/#([A-Za-z0-9_.:-]+)/g, (_, id: string) => {
          return `#${namespaceId(id, idPrefix)}`;
        }),
      );
    }
    if (selectors.length === 0) continue;
    const declarations = filterStyleDeclarations(match[2]!, idPrefix);
    if (declarations === null) continue;
    rules.push(`${selectors.join(", ")} { ${declarations}; }`);
  }
  if (css.slice(consumedUpTo).trim().length > 0) return null;
  return rules.length > 0 ? rules.join("\n") : null;
}

/** Keep the declarations that pass the attribute-path validation, drop the rest. */
function filterStyleDeclarations(value: string, idPrefix: string): string | null {
  if (/[\\@{}]|\/\*/.test(value) || NETWORK_TOKEN.test(value)) return null;
  const declarations: string[] = [];
  for (const declaration of value.split(";")) {
    if (declaration.trim().length === 0) continue;
    const separator = declaration.indexOf(":");
    if (separator <= 0 || declaration.indexOf(":", separator + 1) >= 0) continue;
    const property = declaration.slice(0, separator).trim().toLowerCase();
    const propertyValue = declaration.slice(separator + 1).trim();
    if (!ALLOWED_STYLE_PROPERTIES.has(property) || propertyValue.length === 0) continue;
    const safeValue = safeDeclarationValue(property, propertyValue, idPrefix);
    if (safeValue === null) continue;
    declarations.push(`${property}: ${safeValue}`);
  }
  return declarations.length > 0 ? declarations.join("; ") : null;
}

function escapeRegExp(value: string): string {
  return value.replaceAll(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function sanitizeStyle(value: string, idPrefix: string): string | null {
  if (/[\\@{}]|\/\*/.test(value) || NETWORK_TOKEN.test(value)) return null;
  const declarations: string[] = [];
  for (const declaration of value.split(";")) {
    if (declaration.trim().length === 0) continue;
    const separator = declaration.indexOf(":");
    if (separator <= 0 || declaration.indexOf(":", separator + 1) >= 0) return null;
    const property = declaration.slice(0, separator).trim().toLowerCase();
    const propertyValue = declaration.slice(separator + 1).trim();
    if (!ALLOWED_STYLE_PROPERTIES.has(property) || propertyValue.length === 0) return null;
    const safeValue = safeDeclarationValue(property, propertyValue, idPrefix);
    if (safeValue === null) return null;
    declarations.push(`${property}: ${safeValue}`);
  }
  return declarations.length > 0 ? declarations.join("; ") : null;
}

function safeDeclarationValue(
  property: string,
  propertyValue: string,
  idPrefix: string,
): string | null {
  if (LOCAL_REFERENCE_ATTRIBUTES.has(property)) {
    return propertyValue === "none" ? propertyValue : namespaceLocalUrl(propertyValue, idPrefix);
  }
  if (PAINT_ATTRIBUTES.has(property)) return safePaint(propertyValue, idPrefix);
  return safeScalar(propertyValue) ? propertyValue : null;
}

function safePaint(value: string, idPrefix: string): string | null {
  if (value.includes("\\") || NETWORK_TOKEN.test(value)) return null;
  if (/url\s*\(/i.test(value)) return namespaceLocalUrl(value, idPrefix);
  return safeScalar(value) ? value : null;
}

function namespaceLocalFragment(value: string, idPrefix: string): string | null {
  if (!LOCAL_FRAGMENT.test(value)) return null;
  return `#${namespaceId(value.slice(1), idPrefix)}`;
}

function namespaceLocalUrl(value: string, idPrefix: string): string | null {
  if (!LOCAL_URL.test(value)) return null;
  const fragment = value.slice(value.indexOf("#") + 1, value.lastIndexOf(")")).trim();
  return `url(#${namespaceId(fragment, idPrefix)})`;
}

function namespaceId(value: string, idPrefix: string): string {
  return value.startsWith(idPrefix) ? value : `${idPrefix}${value}`;
}

function svgIdPrefix(namespace: string): string {
  const encoded = [...new TextEncoder().encode(namespace)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
  return `spec-block-${encoded}-`;
}

function safeScalar(value: string): boolean {
  return (
    !value.includes("\\") && !NETWORK_TOKEN.test(value) && !/url\s*\(|var\s*\(|[<>]/i.test(value)
  );
}

function safeIdentifier(value: string): boolean {
  return value.length <= 256 && /^[A-Za-z0-9_.:-]+$/.test(value);
}

function containsControlCharacter(value: string): boolean {
  for (const character of value) {
    const code = character.codePointAt(0);
    if (code !== undefined && (code < 32 || code === 127)) return true;
  }
  return false;
}

function safeClassList(value: string): boolean {
  return value.length <= 1024 && value.split(/\s+/).every(safeIdentifier);
}

export function assertSafeMermaidSource(source: string): void {
  const unsafe =
    /%%\s*\{|\bclick\s+|@\{[^}]*\b(?:img|image|icon)\s*:|!\[[^\]]*]\s*\(|<\s*(?:a|img|image)\b|\b(?:href|xlink:href)\s*[:=]|@import|url\s*\(|(?:https?|ftp|file|blob|data)\s*:|\/\/|\\(?:[0-9a-f]{1,6}\s?|.)/i;
  if (unsafe.test(source)) {
    throw new Error("The Mermaid source contains a link, image, directive, or network value.");
  }
}

export function assertSafeD2Source(source: string): void {
  const unsafe =
    /\b(?:icon|link)\s*:|!\[[^\]]*]\s*\(|@import|url\s*\(|(?:https?|ftp|file|blob|data|javascript|vbscript)\s*:|\/\/|\\(?:[0-9a-f]{1,6}\s?|.)/i;
  if (unsafe.test(source)) {
    throw new Error("The D2 source contains a link, image, or network value.");
  }
}

export function assertNoNetworkValues(value: unknown, path = "source"): void {
  if (typeof value === "string") {
    if (
      NETWORK_TOKEN.test(value) ||
      /url\s*\(|!\[[^\]]*]\s*\(|<\s*(?:a|img|image)\b|\\(?:[0-9a-f]{1,6}\s?|.)/i.test(value)
    ) {
      throw new Error(`The Flint value at ${path} can start a network request.`);
    }
    return;
  }
  if (Array.isArray(value)) {
    value.forEach((child, index) => assertNoNetworkValues(child, `${path}[${index}]`));
    return;
  }
  if (!isRecord(value)) return;
  for (const [key, child] of Object.entries(value)) {
    if (
      key === "$schema" &&
      typeof child === "string" &&
      /^https:\/\/vega\.github\.io\/schema\/vega(?:-lite)?\/v[0-9]+\.json$/.test(child)
    ) {
      continue;
    }
    const normalizedKey = key.toLowerCase().replaceAll(/[^a-z]/g, "");
    if (["url", "href", "src", "image", "images", "icon"].includes(normalizedKey)) {
      throw new Error(`The Flint field at ${path}.${key} can start a network request.`);
    }
    assertNoNetworkValues(child, `${path}.${key}`);
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
