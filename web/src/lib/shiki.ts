import type { HighlighterCore, ThemedTokenWithVariants } from "shiki/types";

import { ENGRAMS_DARK_THEME, ENGRAMS_LIGHT_THEME } from "./syntax-theme";

// The app's single Shiki highlighter: a core instance carrying our two themes
// (syntax-theme.ts) and NO grammars until a fence asks for one.
//
// Every import below is dynamic, so nothing here reaches the entry bundle — the
// engine, the language index and each grammar land in their own chunks, fetched
// the first time a block of that language renders. That matters because the
// transcript is the busiest surface in the app: it must cost nothing until a
// model actually emits a fence.
//
// The JS regex engine (not oniguruma) means a fence never fetches the 600KB
// WASM chunk — the diff viewer still ships one, but reading a transcript does
// not pay for it. `forgiving` lets an exotic grammar drop the handful of
// patterns the JS engine cannot compile rather than throw: a partly highlighted
// block beats a plain one, and a plain one beats an error.

export type SyntaxLines = ThemedTokenWithVariants[][];

export const SYNTAX_THEMES = { light: "engrams-light", dark: "engrams-dark" } as const;

// Tokenizing is synchronous main-thread work. A fence this long is a dumped log
// or a vendored file, where colour buys nothing and a dropped frame costs plenty.
const MAX_HIGHLIGHT_CHARS = 100_000;

let highlighterPromise: Promise<HighlighterCore> | null = null;

function getHighlighter(): Promise<HighlighterCore> {
  highlighterPromise ??= (async () => {
    const [{ createHighlighterCore }, { createJavaScriptRegexEngine }] = await Promise.all([
      import("shiki/core"),
      import("shiki/engine/javascript"),
    ]);
    return createHighlighterCore({
      themes: [ENGRAMS_LIGHT_THEME, ENGRAMS_DARK_THEME],
      langs: [],
      engine: createJavaScriptRegexEngine({ forgiving: true }),
    });
  })();
  return highlighterPromise;
}

// Fence tag -> whether that grammar is loaded. Shiki registers a grammar's own
// aliases, so `py` and `python` resolve to one loaded grammar; the map is keyed
// by the tag we were given, so a miss is remembered too.
const languages = new Map<string, Promise<boolean>>();

async function ensureLanguage(lang: string): Promise<boolean> {
  let pending = languages.get(lang);
  if (!pending) {
    pending = (async () => {
      const [{ bundledLanguages }, highlighter] = await Promise.all([
        import("shiki/langs"),
        getHighlighter(),
      ]);
      if (highlighter.getLoadedLanguages().includes(lang)) return true;
      // `hasOwn` is the guard that makes the cast sound: the tag is a
      // BundledLanguage id or alias exactly when the record holds it.
      if (!Object.hasOwn(bundledLanguages, lang)) return false;
      const loader = bundledLanguages[lang as keyof typeof bundledLanguages];
      await highlighter.loadLanguage(loader);
      return true;
    })().catch(() => false);
    languages.set(lang, pending);
  }
  return pending;
}

// Results are memoized, so a remount, a scroll back, or a re-render of the same
// message does not pay tokenization twice. Bounded: a page of fences stays
// cached, a stream of them does not pin memory.
const cache = new Map<string, Promise<SyntaxLines | null>>();
const CACHE_LIMIT = 64;

/**
 * Tokenize `code` against both themes at once. Each token carries a light and a
 * dark colour, so a theme flip is a CSS variable swap, not a re-highlight.
 *
 * Resolves to `null` when the language is unknown, the fence is too long, or
 * Shiki fails — every one of which the caller renders as plain text.
 */
export function highlight(code: string, lang: string): Promise<SyntaxLines | null> {
  if (!lang || lang === "unknown" || code.length > MAX_HIGHLIGHT_CHARS) {
    return Promise.resolve(null);
  }
  const key = `${lang} ${code}`;
  let pending = cache.get(key);
  if (!pending) {
    pending = tokenize(code, lang);
    cache.set(key, pending);
    if (cache.size > CACHE_LIMIT) {
      const oldest = cache.keys().next();
      if (!oldest.done) cache.delete(oldest.value);
    }
  }
  return pending;
}

async function tokenize(code: string, lang: string): Promise<SyntaxLines | null> {
  try {
    if (!(await ensureLanguage(lang))) return null;
    const highlighter = await getHighlighter();
    return highlighter.codeToTokensWithThemes(code, { lang, themes: SYNTAX_THEMES });
  } catch {
    return null;
  }
}
