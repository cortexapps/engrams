// The shared artifact media model: one place that decides how a stored
// media type renders (the session thread and the /artifacts pages both
// switch on it) and builds the byte URLs.

import {
  AudioLines,
  File,
  FileCode2,
  FileText,
  Film,
  Image as ImageIcon,
  NotebookText,
  type LucideIcon,
} from "lucide-react";

import { API_BASE } from "./base";

export type MediaKind = "image" | "video" | "audio" | "html" | "markdown" | "text" | "binary";

const TEXT_TYPES = new Set(["text/plain", "text/csv", "application/json", "application/xml"]);

export function mediaKind(mediaType: string): MediaKind {
  if (mediaType === "text/html" || mediaType === "image/svg+xml") return "html";
  if (mediaType === "text/markdown") return "markdown";
  if (mediaType.startsWith("image/")) return "image";
  if (mediaType.startsWith("video/")) return "video";
  if (mediaType.startsWith("audio/")) return "audio";
  if (TEXT_TYPES.has(mediaType)) return "text";
  return "binary";
}

export const KIND_GLYPHS: Record<MediaKind, LucideIcon> = {
  image: ImageIcon,
  video: Film,
  audio: AudioLines,
  html: FileCode2,
  markdown: NotebookText,
  text: FileText,
  binary: File,
};

/** Session-scoped byte URL (the transcript's inline media; cookie auth). */
export function sessionArtifactBytesUrl(sessionId: string, artifactId: string): string {
  return `${API_BASE}/sessions/${sessionId}/artifacts/${artifactId}`;
}

/** Cross-session byte URL. `token` (a short-lived capability from
 * ArtifactRecord.raw_url) is only needed from opaque-origin contexts
 * (sandboxed iframes); cookie auth covers everything else. */
export function artifactBytesUrl(
  id: string,
  opts: { v?: number; token?: string; theme?: string } = {},
): string {
  const params = new URLSearchParams();
  if (opts.v !== undefined) params.set("v", String(opts.v));
  if (opts.token !== undefined) params.set("token", opts.token);
  if (opts.theme !== undefined) params.set("theme", opts.theme);
  const qs = params.toString();
  return `${API_BASE}/artifacts/${encodeURIComponent(id)}${qs ? `?${qs}` : ""}`;
}

/** The stable, shareable page URL (GitHub-style optional pretty suffix). */
export function artifactPageUrl(id: string, title?: string): string {
  const slug = title ? slugify(title) : "";
  return `/artifacts/${encodeURIComponent(id)}${slug ? `/${slug}` : ""}`;
}

export function slugify(text: string): string {
  return text
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 60);
}

export function extensionOf(fileName: string): string {
  const dot = fileName.lastIndexOf(".");
  return dot > 0 ? fileName.slice(dot + 1).toLowerCase() : "";
}
