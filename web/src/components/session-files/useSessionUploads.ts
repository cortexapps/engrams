import { useCallback, useRef, useState } from "react";
import { sha256 } from "@noble/hashes/sha2.js";
import { bytesToHex } from "@noble/hashes/utils.js";

export const MAX_UPLOAD_BYTES = 512 * 1024 * 1024;
export const CANONICAL_UPLOAD_PATH_SOURCE = String.raw`\/tmp\/uploads\/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\/[A-Za-z0-9._-]{1,255}`;
export const CANONICAL_UPLOAD_PATH = new RegExp(`^${CANONICAL_UPLOAD_PATH_SOURCE}$`);

export type UploadStatus = "pending" | "hashing" | "uploading" | "uploaded" | "error";

export interface UploadToken {
  id: string;
  name: string;
  path: string;
  file?: File;
  sizeBytes?: number;
  sha256?: string;
  status: UploadStatus;
  progress: number;
  error?: string;
}

export function sanitizeUploadName(name: string): string {
  const mapped = [...name]
    .map((character) => (/^[A-Za-z0-9._-]$/.test(character) ? character : "_"))
    .slice(0, 255)
    .join("");
  return mapped && mapped !== "." && mapped !== ".." ? mapped : "upload";
}

export function tokenForFile(file: File): UploadToken {
  const id = crypto.randomUUID();
  const name = sanitizeUploadName(file.name);
  return {
    id,
    name,
    path: `/tmp/uploads/${id}/${name}`,
    file,
    sizeBytes: file.size,
    status: "pending",
    progress: 0,
  };
}

async function hashFile(file: File, progress: (value: number) => void): Promise<string> {
  const hash = sha256.create();
  const reader = file.stream().getReader();
  let read = 0;
  try {
    while (true) {
      const result = await reader.read();
      if (result.done) break;
      hash.update(result.value);
      read += result.value.byteLength;
      progress(file.size === 0 ? 1 : read / file.size);
    }
  } finally {
    reader.releaseLock();
  }
  return bytesToHex(hash.digest());
}

async function uploadFile(
  sessionId: string,
  token: UploadToken,
  update: (patch: Partial<UploadToken>) => void,
): Promise<UploadToken> {
  if (!token.file) return token;
  if (token.file.size > MAX_UPLOAD_BYTES) {
    throw new Error(`file exceeds ${MAX_UPLOAD_BYTES} bytes`);
  }
  update({ status: "hashing", progress: 0, error: undefined });
  const digest = await hashFile(token.file, (progress) => update({ progress: progress * 0.2 }));
  update({ status: "uploading", sha256: digest, progress: 0.2 });
  const payload = await new Promise<{
    path?: string;
    size_bytes?: number;
    sha256?: string;
    error?: string;
    status: number;
  }>((resolve, reject) => {
    const request = new XMLHttpRequest();
    request.open(
      "POST",
      `/api/v1/sessions/${encodeURIComponent(sessionId)}/uploads?upload_id=${encodeURIComponent(token.id)}&file_name=${encodeURIComponent(token.name)}`,
    );
    request.withCredentials = true;
    request.setRequestHeader("content-type", "application/octet-stream");
    request.setRequestHeader("x-upload-size", String(token.file!.size));
    request.setRequestHeader("x-upload-sha256", digest);
    request.upload.onprogress = (event) => {
      if (!event.lengthComputable) return;
      update({ progress: 0.2 + 0.8 * (event.total === 0 ? 1 : event.loaded / event.total) });
    };
    request.onerror = () => reject(new Error("upload connection failed"));
    request.onabort = () => reject(new Error("upload cancelled"));
    request.onload = () => {
      try {
        resolve({
          ...(JSON.parse(request.responseText) as {
            path?: string;
            size_bytes?: number;
            sha256?: string;
            error?: string;
          }),
          status: request.status,
        });
      } catch {
        reject(new Error(`upload returned HTTP ${request.status}`));
      }
    };
    request.send(token.file);
  });
  if (
    payload.status < 200 ||
    payload.status >= 300 ||
    payload.path !== token.path ||
    payload.sha256 !== digest
  ) {
    throw new Error(payload.error ?? "upload verification failed");
  }
  return {
    ...token,
    sha256: digest,
    sizeBytes: payload.size_bytes ?? token.file.size,
    status: "uploaded",
    progress: 1,
    error: undefined,
  };
}

export function serializeComposer(text: string, tokens: readonly UploadToken[]): string {
  let serialized = text.trim();
  for (const token of tokens) {
    if (serialized.includes(token.path)) continue;
    serialized = [serialized, token.path].filter(Boolean).join("\n");
  }
  return serialized;
}

export function useSessionUploads(sessionId?: string) {
  const [tokens, setTokens] = useState<UploadToken[]>([]);
  const tokensRef = useRef(tokens);
  tokensRef.current = tokens;

  const patchToken = useCallback((id: string, patch: Partial<UploadToken>) => {
    setTokens((current) =>
      current.map((token) => (token.id === id ? { ...token, ...patch } : token)),
    );
  }, []);

  const runUpload = useCallback(
    async (targetSessionId: string, token: UploadToken): Promise<UploadToken> => {
      try {
        const complete = await uploadFile(targetSessionId, token, (patch) =>
          patchToken(token.id, patch),
        );
        patchToken(token.id, complete);
        return complete;
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        patchToken(token.id, { status: "error", error: message });
        throw error;
      }
    },
    [patchToken],
  );

  const addFiles = useCallback(
    (files: FileList | readonly File[]) => {
      const added = Array.from(files).map(tokenForFile);
      setTokens((current) => [...current, ...added]);
      if (sessionId) {
        for (const token of added) void runUpload(sessionId, token).catch(() => {});
      }
      return added;
    },
    [runUpload, sessionId],
  );

  const addCanonicalPath = useCallback((path: string): boolean => {
    if (!CANONICAL_UPLOAD_PATH.test(path)) return false;
    const parts = path.split("/");
    const id = parts.at(-2)!;
    setTokens((current) =>
      current.some((token) => token.path === path)
        ? current
        : [
            ...current,
            {
              id,
              name: parts.at(-1)!,
              path,
              status: "uploaded",
              progress: 1,
            },
          ],
    );
    return true;
  }, []);

  const remove = useCallback((id: string) => {
    setTokens((current) => current.filter((token) => token.id !== id));
  }, []);

  const retry = useCallback(
    async (id: string) => {
      if (!sessionId) return;
      const token = tokensRef.current.find((candidate) => candidate.id === id);
      if (token?.file) await runUpload(sessionId, token).catch(() => undefined);
    },
    [runUpload, sessionId],
  );

  const uploadAll = useCallback(
    async (targetSessionId: string): Promise<UploadToken[]> => {
      const output: UploadToken[] = [];
      for (const token of tokensRef.current) {
        output.push(token.status === "uploaded" ? token : await runUpload(targetSessionId, token));
      }
      return output;
    },
    [runUpload],
  );

  return {
    tokens,
    addFiles,
    addCanonicalPath,
    remove,
    retry,
    uploadAll,
    clear: () => setTokens([]),
    busy: tokens.some((token) => token.status === "hashing" || token.status === "uploading"),
    ready: tokens.every((token) => token.status === "uploaded"),
  };
}
