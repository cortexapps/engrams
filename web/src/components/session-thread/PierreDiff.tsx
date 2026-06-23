import { parseDiffFromFile } from "@pierre/diffs";
import { FileDiff } from "@pierre/diffs/react";
import { useMemo } from "react";

// ADR 0054 Flavor A: the heavy diff renderer, isolated in its own module so
// `FileChangePart` can `React.lazy()` it — Pierre pulls in Shiki (syntax
// grammars + a WASM highlighter), which we keep OUT of the main bundle until a
// user actually expands a diff. `parseDiffFromFile` computes a FileDiffMetadata
// from the before/after contents (jsdiff under the hood); `FileDiff` renders
// it. A Write is a diff from empty → content (all-green); an Edit is the
// joined old → joined new.

export interface PierreDiffProps {
  /** File path — drives the diff header label and Shiki language inference. */
  path: string;
  /** Old contents (empty for a write). */
  before: string;
  /** New contents. */
  after: string;
}

export default function PierreDiff({ path, before, after }: PierreDiffProps) {
  const fileDiff = useMemo(
    () => parseDiffFromFile({ name: path, contents: before }, { name: path, contents: after }),
    [path, before, after],
  );
  return (
    <FileDiff
      fileDiff={fileDiff}
      // We render our own header row (path + counts), so suppress Pierre's.
      // A single small diff needs no worker pool / provider.
      options={{ diffStyle: "unified", themeType: "system", disableFileHeader: true }}
      disableWorkerPool
    />
  );
}
