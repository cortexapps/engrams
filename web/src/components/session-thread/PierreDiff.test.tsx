import { render, waitFor } from "@testing-library/react";
import { resolveTheme } from "@pierre/diffs";
import { expect, test } from "vitest";

import PierreDiff from "./PierreDiff";
import { ThemeProvider } from "@/components/theme-provider";
import { SYNTAX_THEME_NAMES } from "@/lib/syntax-theme";

// The diff viewer keeps its own Shiki instance, so the ONE thing holding it to
// the same palette as a fenced code block is the theme we register with it.
// These cover both halves of that: Pierre resolves the theme we registered, and
// what it paints is our palette, on the ground the APP chose.

const BEFORE = 'def greet(name):\n    return "hi"\n';
const AFTER = 'def greet(name: str) -> str:\n    return f"hello {name}"\n';

async function renderDiff() {
  const { container } = render(
    <ThemeProvider>
      <PierreDiff path="app/greet.py" before={BEFORE} after={AFTER} />
    </ThemeProvider>,
  );
  const shadow = () => container.querySelector("diffs-container")?.shadowRoot?.innerHTML ?? "";
  await waitFor(
    () => {
      expect(shadow()).toContain("hello");
    },
    { timeout: 10_000 },
  );
  return shadow().toLowerCase();
}

test("registers the engrams themes with Pierre", async () => {
  for (const name of [SYNTAX_THEME_NAMES.light, SYNTAX_THEME_NAMES.dark]) {
    const theme = await resolveTheme(name);
    expect(theme.name).toBe(name);
    // Pierre reads its add/delete/modify colours off these keys. Without them
    // an expanded diff silently falls back to Pierre's stock green and red and
    // stops matching the +N / −N counts in its own header.
    expect(theme.colors?.["gitDecoration.addedResourceForeground"]).toMatch(/^#/);
    expect(theme.colors?.["gitDecoration.deletedResourceForeground"]).toMatch(/^#/);
  }
});

test("paints a diff in the engrams palette", async () => {
  const html = await renderDiff();

  // Keyword violet and string emerald — the same values a ```python fence uses.
  expect(html).toContain("#892f84");
  expect(html).toContain("#0f6a31");
  // Pierre's stock add/delete green and red are gone.
  expect(html).not.toContain("#0dbe4e");
  expect(html).not.toContain("#ff2e3f");
});

test("follows the app's theme, not the operating system's", async () => {
  expect(await renderDiff()).toContain("color-scheme: light");

  // The app stores an explicit choice and never consults prefers-color-scheme.
  // Pierre's own `themeType: "system"` does, which painted a dark diff inside a
  // light page whenever the two disagreed.
  localStorage.setItem("engrams-theme", "dark");
  expect(await renderDiff()).toContain("color-scheme: dark");
});
