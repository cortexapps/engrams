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

  const output = document.implementation.createDocument(SVG_NAMESPACE, "svg", null);
  const root = copyElement(parsed.documentElement, output, output, idPrefix);
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
): Element | null {
  const name = source.localName.toLowerCase();
  if (source.namespaceURI !== SVG_NAMESPACE || !ALLOWED_ELEMENTS.has(name)) {
    if (name === "a" && source.namespaceURI === SVG_NAMESPACE) {
      copyChildren(source, parent, output, idPrefix);
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
  copyChildren(source, target, output, idPrefix);
  return target;
}

function copyChildren(
  source: Element,
  parent: Element | Document,
  output: XMLDocument,
  idPrefix: string,
): void {
  for (const child of source.childNodes) {
    if (child.nodeType === Node.ELEMENT_NODE) {
      copyElement(child as Element, parent, output, idPrefix);
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
    const safeValue = LOCAL_REFERENCE_ATTRIBUTES.has(property)
      ? propertyValue === "none"
        ? propertyValue
        : namespaceLocalUrl(propertyValue, idPrefix)
      : PAINT_ATTRIBUTES.has(property)
        ? safePaint(propertyValue, idPrefix)
        : safeScalar(propertyValue)
          ? propertyValue
          : null;
    if (safeValue === null) return null;
    declarations.push(`${property}: ${safeValue}`);
  }
  return declarations.length > 0 ? declarations.join("; ") : null;
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
