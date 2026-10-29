// Each docs page as Markdown at <page>.md, for agents and for "copy as
// Markdown". The docs home is cards in MDX, so its Markdown is the llms.txt index.
import type { APIRoute, GetStaticPaths } from "astro";
import { getCollection, type CollectionEntry } from "astro:content";
import { isHome, llmsIndex, pageMarkdown } from "../lib/markdown";

export const getStaticPaths = (async () =>
  (await getCollection("docs"))
    .filter((doc) => !doc.data.draft)
    .map((doc) => ({ params: { slug: doc.id }, props: { doc } }))) satisfies GetStaticPaths;

export const GET: APIRoute<{ doc: CollectionEntry<"docs"> }> = async ({ props, site }) => {
  const text = isHome(props.doc) ? await llmsIndex(site!) : pageMarkdown(props.doc, site!);
  return new Response(text, { headers: { "Content-Type": "text/markdown; charset=utf-8" } });
};
