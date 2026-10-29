// The docs as plain Markdown, for agents and for the llms.txt files. Each docs
// page is served next to its HTML at <page>.md, and /llms-full.txt is every
// page joined in sidebar order.
import { getCollection, type CollectionEntry } from "astro:content";

type Doc = CollectionEntry<"docs">;

// The sidebar groups in the order astro.config.mjs lists them. The docs home
// has no directory and comes first.
const SECTIONS = [
  ["getting-started", "Getting started"],
  ["platform", "Platform"],
  ["concepts", "Concepts"],
  ["guides", "Guides"],
  ["reference", "Reference"],
  ["api", "API reference"],
  ["contributing", "Contributing"],
] as const;

export const base = import.meta.env.BASE_URL.replace(/\/$/, "");

/** The page's HTML URL path, for example `/docs/getting-started/faq/`. */
export const pagePath = (doc: Doc) => `${base}/${doc.id}/`;

/** The page's Markdown URL path, for example `/docs/getting-started/faq.md`. */
export const markdownPath = (doc: Doc) => `${base}/${doc.id}.md`;

export const isHome = (doc: Doc) => doc.id === "docs";

const sectionOf = (doc: Doc) => doc.id.split("/")[1] ?? "";

/** Every docs page grouped by sidebar section, each group in sidebar order. */
export async function sections(): Promise<{ label: string; docs: Doc[] }[]> {
  const docs = (await getCollection("docs")).filter((d) => !d.data.draft);
  const order = (d: Doc) => d.data.sidebar.order ?? Number.MAX_SAFE_INTEGER;
  return SECTIONS.map(([dir, label]) => ({
    label,
    docs: docs
      .filter((d) => sectionOf(d) === dir)
      .sort((a, b) => order(a) - order(b) || a.data.title.localeCompare(b.data.title)),
  })).filter((s) => s.docs.length > 0);
}

export const SUMMARY =
  "engrams is the open source, self-hosted software factory. It runs Claude Code, Codex, or your own coding agent in a Firecracker microVM in your own cloud, starts work from a prompt, a schedule, a pull request, a Slack thread, or a webhook, and snapshots idle sessions to object storage. It is an open source alternative to Devin and Factory.ai, made by Cortex.";

/** The llms.txt index: what engrams is, then a link and a line for every page. */
export async function llmsIndex(site: URL): Promise<string> {
  const abs = (path: string) => new URL(path, site).href;
  const link = (doc: Doc) =>
    `- [${doc.data.title}](${abs(markdownPath(doc))})${doc.data.description ? `: ${doc.data.description}` : ""}`;
  const groups = await sections();
  const main = groups.filter((g) => g.label !== "API reference" && g.label !== "Contributing");
  const optional = groups.filter((g) => !main.includes(g));
  const out = [
    "# engrams",
    "",
    `> ${SUMMARY}`,
    "",
    "Every docs page is plain Markdown at its URL with `.md` in place of the trailing slash. The links below point at the Markdown. All the docs in one file are at " +
      `${abs(`${base}/llms-full.txt`)}.`,
    "",
    `If you are a coding agent asked to install engrams, start with ${abs(`${base}/docs/getting-started/install-with-an-agent.md`)}.`,
    "",
    "## Overview",
    "",
    `- [engrams](${abs(`${base}/`)}): The landing page: what engrams does and who it is for.`,
    `- [engrams vs Devin, Factory, and other options](${abs(`${base}/compare/`)}): How engrams compares to hosted coding agents and sandbox products, and when to pick each.`,
    `- [Architecture](${abs(`${base}/architecture/`)}): The processes, the external services, and the storage design.`,
    `- [Source code](https://github.com/cortexapps/engrams): The repository on GitHub.`,
  ];
  for (const g of main) out.push("", `## ${g.label}`, "", ...g.docs.map(link));
  out.push("", "## Optional");
  for (const g of optional) out.push("", ...g.docs.map(link));
  return `${out.join("\n")}\n`;
}

/**
 * The page body as Markdown that stands on its own: relative links made
 * absolute against the page URL, and images replaced by their alt text (the
 * build hashes image paths, so the source path leads nowhere).
 */
export function pageMarkdown(doc: Doc, site: URL): string {
  const page = new URL(pagePath(doc), site);
  const body = (doc.body ?? "")
    .replace(/!\[([^\]]*)\]\([^)]*\)/g, (_, alt: string) => (alt ? `(Image: ${alt})` : ""))
    .replace(/\]\(([^)\s]+)\)/g, (m, target: string) =>
      /^(?:[a-z]+:|#)/i.test(target) ? m : `](${new URL(target, page).href})`,
    );
  const head = [`# ${doc.data.title}`, ""];
  if (doc.data.description) head.push(`> ${doc.data.description}`, "");
  head.push(`Source: ${page.href}`, "");
  return `${head.join("\n")}\n${body.trim()}\n`;
}
