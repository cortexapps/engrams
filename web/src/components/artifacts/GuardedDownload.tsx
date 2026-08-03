import { useState, type ReactNode } from "react";
import { TriangleAlert } from "lucide-react";

import { fmtBytes } from "../transcriptFmt";
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

// A trust gate in front of EVERY download. The files come from an
// agent, and an agent can be steered by content it read (prompt
// injection) into emitting something the user never asked for. A
// per-kind exemption sounds harmless but leaks: transcript HTML/SVG is
// offered for download WITHOUT ever being rendered, and opened from
// disk it runs scripts in a file:// origin with no sandbox (review
// finding on #991). One uniform interstitial is simpler to reason
// about than a taxonomy of "safe enough" types.
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

  const confirm = () => {
    // Same-origin cookie-authed download via a transient anchor — the
    // dialog owned the click, so trigger it programmatically. The empty
    // string keeps the bare `download` attribute when no name exists:
    // an omitted attribute would navigate instead (renderable types are
    // served inline), replacing the SPA with the raw bytes.
    const a = document.createElement("a");
    a.href = url;
    a.download = fileName ?? "";
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
                An agent produced this file. Agents can be manipulated by content they read while
                working, so treat this like an attachment from an unknown sender: do not run
                executables, scripts, or installers from it, and open documents in a viewer you
                trust.
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
