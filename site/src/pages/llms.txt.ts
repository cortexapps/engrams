// The llms.txt index (https://llmstxt.org): what engrams is, and a link to the
// Markdown of every docs page.
import type { APIRoute } from "astro";
import { llmsIndex } from "../lib/markdown";

export const GET: APIRoute = async ({ site }) =>
  new Response(await llmsIndex(site!), { headers: { "Content-Type": "text/plain; charset=utf-8" } });
