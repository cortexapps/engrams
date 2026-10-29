// Every docs page as Markdown in one file, in sidebar order, for an agent that
// wants the whole manual in one request.
import type { APIRoute } from "astro";
import { SUMMARY, pageMarkdown, sections } from "../lib/markdown";

export const GET: APIRoute = async ({ site }) => {
  const pages = (await sections()).flatMap((s) => s.docs).map((d) => pageMarkdown(d, site!));
  const text = [`# engrams\n\n> ${SUMMARY}\n`, ...pages].join("\n---\n\n");
  return new Response(text, { headers: { "Content-Type": "text/plain; charset=utf-8" } });
};
