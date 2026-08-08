import { useRef } from "react";
import { Copy, FileUp, LoaderCircle, RefreshCw, X } from "lucide-react";

import type { UploadToken } from "./useSessionUploads";
import { Button } from "@/components/ui/button";

export interface UploadTrayProps {
  tokens: readonly UploadToken[];
  onFiles: (files: FileList | readonly File[]) => void;
  onRemove: (id: string) => void;
  onRetry: (id: string) => void;
  disabled?: boolean;
}

export function UploadTray({ tokens, onFiles, onRemove, onRetry, disabled }: UploadTrayProps) {
  const input = useRef<HTMLInputElement>(null);
  return (
    <div className="flex min-w-0 flex-wrap items-center gap-1.5">
      <input
        ref={input}
        type="file"
        multiple
        className="hidden"
        onChange={(event) => {
          if (event.target.files) onFiles(event.target.files);
          event.target.value = "";
        }}
      />
      <Button
        type="button"
        variant="ghost"
        size="icon-sm"
        disabled={disabled}
        aria-label="Attach files"
        onClick={() => input.current?.click()}
      >
        <FileUp className="size-4" />
      </Button>
      {tokens.map((token) => (
        <span
          key={token.id}
          className="flex max-w-full items-center gap-1 rounded-md border bg-secondary/60 px-2 py-1 font-mono text-xs"
          title={token.error ?? token.path}
        >
          {(token.status === "hashing" || token.status === "uploading") && (
            <LoaderCircle className="size-3 animate-spin" />
          )}
          <span className="max-w-64 truncate">{token.path}</span>
          <button
            type="button"
            aria-label={`Copy ${token.path}`}
            onClick={() => void navigator.clipboard.writeText(token.path)}
          >
            <Copy className="size-3" />
          </button>
          {token.status === "error" && token.file && (
            <button
              type="button"
              aria-label={`Retry ${token.name}`}
              onClick={() => onRetry(token.id)}
            >
              <RefreshCw className="size-3 text-destructive" />
            </button>
          )}
          <button
            type="button"
            aria-label={`Remove ${token.name}`}
            onClick={() => onRemove(token.id)}
          >
            <X className="size-3" />
          </button>
          {token.progress > 0 && token.progress < 1 && (
            <span className="text-muted-foreground">{Math.round(token.progress * 100)}%</span>
          )}
        </span>
      ))}
    </div>
  );
}
