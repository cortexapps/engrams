import { parseDiffFromFile, registerCustomTheme } from "@pierre/diffs";
import { FileDiff } from "@pierre/diffs/react";
import { useMemo } from "react";

import { useTheme } from "@/components/theme-provider";
import { ENGRAMS_DARK_THEME, ENGRAMS_LIGHT_THEME, SYNTAX_THEME_NAMES } from "@/lib/syntax-theme";

// ADR 0054 Flavor A: the heavy diff renderer, isolated in its own module so
// `FileChangePart` can `React.lazy()` it — Pierre pulls in Shiki (syntax
// grammars + a highlighter), which we keep OUT of the main bundle until a
// user actually expands a diff. `parseDiffFromFile` computes a FileDiffMetadata
// from the before/after contents (jsdiff under the hood); `FileDiff` renders
// it. A Write is a diff from empty → content (all-green); an Edit is the
// joined old → joined new.
//
// The diff runs on the SAME palette as a fenced code block (lib/syntax-theme.ts):
// a Python file reads identically whether the model pasted it or edited it.
// Pierre keeps its own Shiki instance — it has no seam to inject ours — but a
// theme it can be told about, so the two agree on colour if not on machinery.
// Pierre also reads `gitDecoration.*` off the theme for its add/delete/modify
// colours, which is how an expanded diff comes to match the +N / −N counts in
// its own header instead of Pierre's stock green and red.

registerCustomTheme(SYNTAX_THEME_NAMES.light, () => Promise.resolve(ENGRAMS_LIGHT_THEME));
registerCustomTheme(SYNTAX_THEME_NAMES.dark, () => Promise.resolve(ENGRAMS_DARK_THEME));

export interface PierreDiffProps {
  /** File path — drives the diff header label and Shiki language inference. */
  path: string;
  /** Old contents (empty for a write). */
  before: string;
  /** New contents. */
  after: string;
}

export default function PierreDiff({ path, before, after }: PierreDiffProps) {
  // Pierre's own "system" themeType follows prefers-color-scheme, but the app's
  // theme is an explicit stored choice that nothing maps onto `color-scheme` —
  // so "system" rendered a dark diff inside a light page whenever the two
  // disagreed. Read the app's theme instead; it is the only one that decides.
  const { theme } = useTheme();
  const fileDiff = useMemo(
    () => parseDiffFromFile({ name: path, contents: before }, { name: path, contents: after }),
    [path, before, after],
  );
  return (
    <FileDiff
      fileDiff={fileDiff}
      // We render our own header row (path + counts), so suppress Pierre's.
      // A single small diff needs no worker pool / provider.
      options={{
        diffStyle: "unified",
        theme: SYNTAX_THEME_NAMES,
        themeType: theme,
        disableFileHeader: true,
      }}
      disableWorkerPool
    />
  );
}
