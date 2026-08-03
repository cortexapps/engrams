import { useState } from "react";
import { Check, Copy, Globe, Lock, Share2 } from "lucide-react";

import type { ArtifactRecord } from "../../gen/engram/app/v1/artifact_pb";
import { useSetArtifactVisibility } from "../../hooks/useArtifacts";
import { artifactPageUrl } from "../../lib/artifacts";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Text } from "@/components/ui/text";

// The share modal (the one place visibility changes): a state card that
// says plainly who can see the artifact, and one primary action —
// "Share & copy link" when private, "Copy link" (+ a quiet
// "Stop sharing") once shared. The stable page URL is the thing being
// handed out, so sharing and copying travel together.
export function ShareDialog({ artifact }: { artifact: ArtifactRecord }) {
  const setVisibility = useSetArtifactVisibility();
  const [copied, setCopied] = useState(false);
  const [open, setOpen] = useState(false);
  const shared = artifact.visibility === "org";

  const copyLink = () => {
    const link = `${window.location.origin}${artifactPageUrl(artifact.id, artifact.title)}`;
    void navigator.clipboard.writeText(link).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    });
  };

  const shareAndCopy = () => {
    setVisibility.mutate({ id: artifact.id, visibility: "org" }, { onSuccess: copyLink });
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        setOpen(next);
        if (!next) setCopied(false);
      }}
    >
      <DialogTrigger asChild>
        <Button variant="outline" size="sm">
          <Share2 /> Share
        </Button>
      </DialogTrigger>
      <DialogContent className="max-w-md">
        <DialogHeader>
          <DialogTitle>Share artifact</DialogTitle>
          <DialogDescription>
            Sharing gives everyone in your organization a view link. Only you and admins can update,
            unshare, or delete it.
          </DialogDescription>
        </DialogHeader>

        <div className="flex items-center gap-3 rounded-md border px-4 py-3">
          {shared ? (
            <Globe className="size-5 shrink-0 text-primary" />
          ) : (
            <Lock className="size-5 shrink-0 text-muted-foreground" />
          )}
          <div className="min-w-0">
            <Text as="p" className="text-sm font-medium">
              {shared ? "Shared with the org" : "Unshared"}
            </Text>
            <Text as="p" variant="body" tone="muted" className="text-sm">
              {shared
                ? "Anyone in your organization can view this artifact."
                : "Only you have access."}
            </Text>
          </div>
        </div>

        <DialogFooter>
          {shared ? (
            <>
              <Button
                variant="ghost"
                disabled={setVisibility.isPending}
                onClick={() => setVisibility.mutate({ id: artifact.id, visibility: "private" })}
              >
                Stop sharing
              </Button>
              <Button onClick={copyLink}>
                {copied ? <Check /> : <Copy />} {copied ? "Copied" : "Copy link"}
              </Button>
            </>
          ) : (
            <Button disabled={setVisibility.isPending} onClick={shareAndCopy}>
              {copied ? <Check /> : <Share2 />}
              {copied ? "Link copied" : "Share & copy link"}
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
