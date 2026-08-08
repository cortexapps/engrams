import { useRef } from "react";
import { FileUp } from "lucide-react";

import { Button } from "@/components/ui/button";

export interface UploadButtonProps {
  onFiles: (files: FileList | readonly File[]) => void;
  disabled?: boolean;
}

export function UploadButton({ onFiles, disabled }: UploadButtonProps) {
  const input = useRef<HTMLInputElement>(null);
  return (
    <>
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
    </>
  );
}
