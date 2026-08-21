/** Session file staging over the WriteFile streaming RPC (ADR 0119 phase 2.D).
 *
 * One metadata frame (sessionId, path, sizeBytes, sha256, mode) then a single
 * chunk frame — skipped for empty files. Content-addressed by sha256, so a
 * DBOS step replay re-writes the same bytes idempotently. Errors are captured
 * per file, never thrown: callers decide whether a failed path is fatal.
 *
 * Shared by the review control plane and the automation engine's
 * write_files block; extracted from workflows/review-control-plane.ts.
 */

import { createHash } from "node:crypto";

export interface WriteFileMetadata {
  sessionId: string;
  path: string;
  sizeBytes: bigint;
  sha256: string;
  mode?: number;
}

export type WriteFileFrame =
  | { case: "metadata"; value: WriteFileMetadata }
  | { case: "chunk"; value: Uint8Array };

/** The narrow client surface staging needs — provider-neutral. */
export interface FileStagingClient {
  writeFile(
    input: AsyncIterable<{ frame: WriteFileFrame }>,
  ): Promise<{ path: string; sizeBytes: bigint; sha256: string }>;
}

export interface FileToStage {
  path: string;
  content: Uint8Array;
  mode: number;
}

export interface StagedFileResult {
  path: string;
  ok: boolean;
  error?: string;
}

export async function stageFiles(
  sessions: FileStagingClient,
  sessionId: string,
  files: readonly FileToStage[],
): Promise<StagedFileResult[]> {
  const results: StagedFileResult[] = [];
  for (const file of files) {
    const sha256 = createHash("sha256").update(file.content).digest("hex");
    async function* frames(): AsyncIterable<{ frame: WriteFileFrame }> {
      yield {
        frame: {
          case: "metadata" as const,
          value: {
            sessionId,
            path: file.path,
            sizeBytes: BigInt(file.content.byteLength),
            sha256,
            mode: file.mode,
          },
        },
      };
      if (file.content.byteLength > 0) {
        yield { frame: { case: "chunk" as const, value: file.content } };
      }
    }
    try {
      await sessions.writeFile(frames());
      results.push({ path: file.path, ok: true });
    } catch (error) {
      results.push({
        path: file.path,
        ok: false,
        error: error instanceof Error ? error.message : String(error),
      });
    }
  }
  return results;
}
