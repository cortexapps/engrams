import { useState, type ReactNode } from "react";
import { TriangleAlert } from "lucide-react";

import { fmtBytes } from "../transcriptFmt";
import { mediaKind } from "../../lib/artifacts";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog";

// A trust gate in front of downloads the product cannot vouch for.
// Media and text render in the product first, so a person has seen what
// they are saving; an arbitrary binary is opaque until it is on their
// machine — and it was produced by an agent, which can be steered by
// content it read (prompt injection) into emitting something the user
// never asked for. Renderable kinds pass straight through; the "binary"
// kind gets one honest interstitial.
export function GuardedDownload({
  url,
  mediaType,
  fileName,
  sizeBytes,
  children,
  className,
  "aria-label": ariaLabel,
}: {
  url: string;
  mediaType: string;
  fileName?: string;
  sizeBytes?: number;
  children: ReactNode;
  className?: string;
  "aria-label"?: string;
}) {
  const [open, setOpen] = useState(false);

  if (mediaKind(mediaType) !== "binary") {
    return (
      <a href={url} download={fileName} className={className} aria-label={ariaLabel}>
        {children}
      </a>
    );
  }

  const confirm = () => {
    // Same-origin cookie-authed download via a transient anchor — the
    // dialog owned the click, so trigger it programmatically.
    const a = document.createElement("a");
    a.href = url;
    if (fileName) a.download = fileName;
    document.body.appendChild(a);
    a.click();
    a.remove();
  };

  return (
    <AlertDialog open={open} onOpenChange={setOpen}>
      <AlertDialogTrigger asChild>
        <button type="button" className={className} aria-label={ariaLabel}>
          {children}
        </button>
      </AlertDialogTrigger>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle className="flex items-center gap-2">
            <TriangleAlert className="size-4 text-instrument-caution" />
            Download this file?
          </AlertDialogTitle>
          <AlertDialogDescription asChild>
            <div className="space-y-2">
              <p className="font-mono text-xs">
                {fileName ?? "unnamed file"} · {mediaType}
                {sizeBytes !== undefined ? <> · {fmtBytes(sizeBytes)}</> : null}
              </p>
              <p>
                An agent produced this file, and engrams cannot inspect what is inside it. Agents
                can be manipulated by content they read while working, so treat this like an
                attachment from an unknown sender: do not run executables, scripts, or installers
                from it, and open documents in a viewer you trust.
              </p>
              <p>Only continue if you asked for this file and know what it is.</p>
            </div>
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>Cancel</AlertDialogCancel>
          <AlertDialogAction onClick={confirm}>Download anyway</AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
