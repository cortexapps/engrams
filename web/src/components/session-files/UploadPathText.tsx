import { createContext, useContext, type ReactNode } from "react";
import type { TextMessagePartProps } from "@assistant-ui/react";
import { Download } from "lucide-react";

const UPLOAD_PATH_PATTERN =
  /\/tmp\/uploads\/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\/[A-Za-z0-9._-]{1,255}/g;

export const SessionFileContext = createContext<string | null>(null);

export function uploadPathParts(text: string, sessionId: string | null): ReactNode[] {
  if (!sessionId) return [text];
  const parts: ReactNode[] = [];
  let cursor = 0;
  for (const match of text.matchAll(UPLOAD_PATH_PATTERN)) {
    const index = match.index;
    const path = match[0];
    if (index > cursor) parts.push(text.slice(cursor, index));
    parts.push(
      <a
        key={`${index}:${path}`}
        href={`/api/v1/sessions/${encodeURIComponent(sessionId)}/files?path=${encodeURIComponent(path)}`}
        download={path.split("/").at(-1)}
        className="mx-0.5 inline-flex max-w-full items-center gap-1 rounded-md border bg-secondary/60 px-1.5 py-0.5 align-baseline font-mono text-xs text-primary hover:bg-secondary"
        title={`Download ${path}`}
      >
        <Download className="size-3 shrink-0" />
        <span className="truncate">{path}</span>
      </a>,
    );
    cursor = index + path.length;
  }
  if (cursor < text.length) parts.push(text.slice(cursor));
  return parts.length > 0 ? parts : [text];
}

export function UploadPathText({ text }: TextMessagePartProps) {
  const sessionId = useContext(SessionFileContext);
  return <p className="whitespace-pre-line">{uploadPathParts(text, sessionId)}</p>;
}

export function UploadPathChildren({ children }: { children: ReactNode }) {
  const sessionId = useContext(SessionFileContext);
  if (typeof children !== "string") return children;
  return uploadPathParts(children, sessionId);
}
