import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { DownloadIcon, Loader2 } from "lucide-react";

import { buttonVariants } from "@/components/ui/button";
import { Text } from "@/components/ui/text";
import { GuardedDownload } from "./GuardedDownload";
import { Markdown } from "@/components/Markdown";
import { KIND_GLYPHS, mediaKind, type MediaKind } from "@/lib/artifacts";
import { cn } from "@/lib/utils";
import { ImageWithLightbox } from "./ImageLightbox";

// One viewer for every artifact/shared-file media kind, used by BOTH the
// session thread ("inline") and the /artifacts detail page ("full"):
//   image  → lightboxed img            video/audio → native controls
//   html   → sandboxed iframe (full only; scripts run inside an
//            opaque origin — the tokened src carries auth because a
//            sandboxed frame sends no SameSite cookies)
//   markdown → authed text fetch into the logbook Markdown renderer
//   text   → authed text fetch into a ruled .md-pre block
//   binary → a download affordance
//
// The chrome (cards, toolbars, download buttons) belongs to the caller;
// this component is only the media body.

export interface ArtifactViewerProps {
  /** Cookie-authed byte URL (downloads, img/video src, text fetches). */
  url: string;
  /** Tokened byte URL for the sandboxed HTML iframe (full variant). */
  htmlSrc?: string;
  mediaType: string;
  fileName?: string;
  caption?: string | null;
  variant: "inline" | "full";
}

/** Text bodies above this render as a download instead (a transcript or
 * reading pane is the wrong tool for a 50 MB log). */
const MAX_TEXT_BYTES = 2 * 1024 * 1024;

function useArtifactText(url: string, enabled: boolean) {
  return useQuery({
    queryKey: ["artifact-text", url],
    enabled,
    staleTime: Infinity, // versions are immutable; the URL changes when content does
    queryFn: async () => {
      const res = await fetch(url, { credentials: "include" });
      if (!res.ok) throw new Error(`fetch failed (${res.status})`);
      const length = Number(res.headers.get("content-length") ?? 0);
      if (length > MAX_TEXT_BYTES) throw new Error("too large to preview");
      return res.text();
    },
  });
}

function DownloadPlate({
  url,
  fileName,
  kind,
  mediaType,
  note,
}: {
  url: string;
  fileName?: string;
  kind: MediaKind;
  /** Shown in the download interstitial's file line; every download is
   * gated regardless. */
  mediaType?: string;
  note?: string;
}) {
  const Glyph = KIND_GLYPHS[kind];
  return (
    <div className="flex items-center gap-3 self-start rounded-md border bg-muted/40 px-4 py-3">
      <Glyph className="size-5 shrink-0 text-muted-foreground" />
      <div className="flex flex-col">
        {fileName && (
          <Text as="span" variant="code" className="text-sm">
            {fileName}
          </Text>
        )}
        {note && (
          <Text as="span" variant="body" tone="muted" className="text-xs">
            {note}
          </Text>
        )}
      </div>
      <GuardedDownload
        url={url}
        mediaType={mediaType ?? "text/plain"}
        {...(fileName !== undefined ? { fileName } : {})}
        className={cn(buttonVariants({ variant: "outline", size: "sm" }), "ml-2")}
      >
        <DownloadIcon /> Download
      </GuardedDownload>
    </div>
  );
}

function TextBody({
  url,
  kind,
  fileName,
  variant,
}: {
  url: string;
  kind: "markdown" | "text";
  fileName?: string;
  variant: "inline" | "full";
}) {
  const { data, isPending, error } = useArtifactText(url, true);
  if (isPending) {
    return (
      <div className="flex items-center gap-2 py-4 text-muted-foreground">
        <Loader2 className="size-4 animate-spin" />
        <Text as="span" variant="label">
          loading
        </Text>
      </div>
    );
  }
  if (error || data === undefined) {
    return (
      <DownloadPlate
        url={url}
        fileName={fileName}
        kind={kind}
        note={error instanceof Error ? error.message : "preview unavailable"}
      />
    );
  }
  const body =
    kind === "markdown" ? (
      <Markdown text={data} highlightCode />
    ) : (
      <pre className="md-pre overflow-x-auto">{data}</pre>
    );
  if (variant === "inline") {
    // Clamp in the transcript; the artifact page is the reading surface.
    return <div className="max-h-[24rem] overflow-y-auto pr-1">{body}</div>;
  }
  return kind === "markdown" ? (
    <div className="mx-auto w-full max-w-[72ch] py-6">{body}</div>
  ) : (
    <div className="py-4">{body}</div>
  );
}

/** Full-bleed sandboxed frame for HTML/SVG artifacts. Scripts run, but
 * inside a unique opaque origin (no cookies, no app-origin access) —
 * the response's own CSP enforces the same on direct navigation. */
export function ArtifactFrame({ src, title }: { src: string; title: string }) {
  const [loaded, setLoaded] = useState(false);
  useEffect(() => setLoaded(false), [src]);
  return (
    <div className="relative min-h-0 flex-1">
      {!loaded && (
        <div className="absolute inset-0 flex items-center justify-center text-muted-foreground">
          <Loader2 className="size-5 animate-spin" />
        </div>
      )}
      <iframe
        src={src}
        title={title}
        sandbox="allow-scripts allow-forms allow-modals allow-popups allow-downloads"
        className={cn(
          "size-full border-0 bg-transparent transition-opacity",
          loaded ? "opacity-100" : "opacity-0",
        )}
        onLoad={() => setLoaded(true)}
      />
    </div>
  );
}

export function ArtifactViewer({
  url,
  htmlSrc,
  mediaType,
  fileName,
  caption,
  variant,
}: ArtifactViewerProps) {
  const kind = mediaKind(mediaType);

  switch (kind) {
    case "image":
      return (
        <ImageWithLightbox
          src={url}
          alt={caption ?? fileName ?? "shared image"}
          caption={caption}
          className={cn(
            "w-auto max-w-full rounded-md border cursor-zoom-in",
            variant === "inline" ? "max-h-[32rem] self-start" : "max-h-[70vh] mx-auto",
          )}
        />
      );
    case "video":
      return (
        // eslint-disable-next-line jsx-a11y/media-has-caption
        <video
          src={url}
          controls
          className={cn(
            "w-auto max-w-full rounded-md border",
            variant === "inline" ? "max-h-[32rem] self-start" : "max-h-[70vh] mx-auto",
          )}
        />
      );
    case "audio":
      // eslint-disable-next-line jsx-a11y/media-has-caption
      return <audio src={url} controls className="w-full max-w-md" />;
    case "html":
      if (variant === "full") {
        return <ArtifactFrame src={htmlSrc ?? url} title={fileName ?? "artifact"} />;
      }
      // In the transcript an HTML file is a download; the artifact page
      // is where it runs (sandboxed).
      return <DownloadPlate url={url} fileName={fileName} kind="html" mediaType={mediaType} />;
    case "markdown":
    case "text":
      return <TextBody url={url} kind={kind} fileName={fileName} variant={variant} />;
    case "binary":
      return <DownloadPlate url={url} fileName={fileName} kind="binary" mediaType={mediaType} />;
  }
}
