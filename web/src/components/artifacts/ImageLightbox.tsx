import { useState } from "react";
import { ExternalLinkIcon, XIcon } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Dialog, DialogClose, DialogContent } from "@/components/ui/dialog";

// Click-to-zoom image with the transcript's lightbox dialog — extracted
// from SystemMessage so the /artifacts pages share it.
export function ImageWithLightbox({
  src,
  alt,
  caption,
  className,
}: {
  src: string;
  alt: string;
  caption?: string | null;
  className?: string;
}) {
  const [open, setOpen] = useState(false);
  return (
    <>
      <img
        src={src}
        alt={alt}
        className={
          className ?? "w-auto max-h-[32rem] max-w-full self-start rounded-md border cursor-zoom-in"
        }
        onClick={() => setOpen(true)}
      />
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent
          showCloseButton={false}
          className="max-w-[min(95vw,900px)] p-0 gap-0 flex flex-col overflow-hidden"
        >
          <div className="flex shrink-0 items-center justify-between border-b px-3 py-2">
            <a
              href={src}
              target="_blank"
              rel="noreferrer"
              className="flex items-center gap-1.5 text-xs text-muted-foreground transition-colors hover:text-foreground"
              onClick={(e) => e.stopPropagation()}
            >
              <ExternalLinkIcon className="size-3.5" />
              Open in new tab
            </a>
            <DialogClose asChild>
              <Button variant="ghost" size="icon" className="size-7">
                <XIcon className="size-4" />
                <span className="sr-only">Close</span>
              </Button>
            </DialogClose>
          </div>
          <div className="overflow-y-auto">
            <img src={src} alt={alt} className="w-full" />
          </div>
          {caption && <p className="shrink-0 border-t px-3 py-2 text-sm">{caption}</p>}
        </DialogContent>
      </Dialog>
    </>
  );
}
