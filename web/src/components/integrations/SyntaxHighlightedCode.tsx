import { useEffect, useState, type CSSProperties } from "react";
import { createHighlighterCore, type ThemedTokenWithVariants } from "shiki/core";
import { createJavaScriptRegexEngine } from "shiki/engine/javascript";
import shellscript from "shiki/langs/shellscript.mjs";
import terraform from "shiki/langs/terraform.mjs";
import githubDark from "shiki/themes/github-dark-default.mjs";
import githubLight from "shiki/themes/github-light-default.mjs";

export type SetupCodeLanguage = "terraform" | "shellscript";

const highlighter = createHighlighterCore({
  themes: [githubLight, githubDark],
  langs: [terraform, shellscript],
  engine: createJavaScriptRegexEngine(),
});

export default function SyntaxHighlightedCode({
  language,
  value,
}: {
  language: SetupCodeLanguage;
  value: string;
}) {
  const [lines, setLines] = useState<ThemedTokenWithVariants[][] | null>(null);

  useEffect(() => {
    let active = true;
    setLines(null);
    void highlighter
      .then((instance) => {
        if (!active) return;
        setLines(
          instance.codeToTokensWithThemes(value, {
            lang: language,
            themes: {
              light: "github-light-default",
              dark: "github-dark-default",
            },
          }),
        );
      })
      .catch(() => {
        if (active) setLines(null);
      });
    return () => {
      active = false;
    };
  }, [language, value]);

  return (
    <code data-language={language} data-highlighted={lines ? "true" : "false"}>
      {lines
        ? lines.map((line, lineIndex) => (
            <span key={lineIndex}>
              {line.map((token) => (
                <SyntaxToken key={token.offset} token={token} />
              ))}
              {lineIndex < lines.length - 1 ? "\n" : null}
            </span>
          ))
        : value}
    </code>
  );
}

function SyntaxToken({ token }: { token: ThemedTokenWithVariants }) {
  const light = token.variants.light;
  const dark = token.variants.dark;
  const fontStyle = light?.fontStyle ?? dark?.fontStyle ?? 0;
  const style = {
    "--syntax-light": light?.color ?? "currentColor",
    "--syntax-dark": dark?.color ?? "currentColor",
    fontStyle: fontStyle & 1 ? "italic" : undefined,
    fontWeight: fontStyle & 2 ? 700 : undefined,
    textDecoration: fontStyle & 4 ? "underline" : undefined,
  } as CSSProperties;

  return (
    <span className="text-[var(--syntax-light)] dark:text-[var(--syntax-dark)]" style={style}>
      {token.content}
    </span>
  );
}
