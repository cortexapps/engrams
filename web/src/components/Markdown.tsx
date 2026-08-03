import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";

import { CodeBlock } from "./CodeBlock";

// Render a completed assistant/system message as Markdown, styled
// in-system (ADR 0030 §2g). LLMs emit Markdown; every serious AI UI
// renders it. We map react-markdown's elements onto the notebook
// `.md-*` classes in theme.css (mono code, square bullets,
// hairline-boxed inline code, verdigris-ruled code blocks).
//
// SAFETY: we deliberately do NOT enable `rehype-raw`, so any raw HTML
// in the model's output is treated as literal text rather than parsed
// — there is no `dangerouslySetInnerHTML` path, nothing to inject, and
// no separate sanitizer is needed. GFM (tables, strikethrough,
// autolinks, task lists) is enabled via remark-gfm.
//
// Used only for *completed* messages. While a message is still
// streaming we render plain text and only swap to this once it
// completes — though today our `agent_message` events arrive
// per-final-message (the harness consolidates), so messages are
// already complete on arrival.

const COMPONENTS: Components = {
  h1: ({ children }) => <h1 className="md-h">{children}</h1>,
  h2: ({ children }) => <h2 className="md-h">{children}</h2>,
  h3: ({ children }) => <h3 className="md-h">{children}</h3>,
  h4: ({ children }) => <h4 className="md-h">{children}</h4>,
  h5: ({ children }) => <h5 className="md-h">{children}</h5>,
  h6: ({ children }) => <h6 className="md-h">{children}</h6>,
  ul: ({ children }) => <ul className="md-ul">{children}</ul>,
  ol: ({ children }) => <ol className="md-ol">{children}</ol>,
  code: ({ children, ...props }) => (
    <code className="md-code" {...props}>
      {children}
    </code>
  ),
  pre: ({ children }) => <pre className="md-pre">{children}</pre>,
  a: ({ children, href }) => (
    <a className="md-link" href={href} target="_blank" rel="noopener noreferrer">
      {children}
    </a>
  ),
  blockquote: ({ children }) => <blockquote className="md-quote">{children}</blockquote>,
  hr: () => <hr className="md-hr" />,
};

// Opt-in variant: fenced code blocks with a declared language get lazy
// Shiki highlighting (CodeBlock). Chat transcripts keep the plain
// COMPONENTS — highlighting every streamed message would cost a bundle
// chunk + tokenization work the transcript doesn't need; document
// surfaces (markdown artifacts) opt in.
const HIGHLIGHT_COMPONENTS: Components = {
  ...COMPONENTS,
  code: ({ children, className, ...props }) => {
    const lang = /language-(\w+)/.exec(className ?? "")?.[1];
    if (lang && typeof children === "string") {
      return <CodeBlock code={children.replace(/\n$/, "")} language={lang} />;
    }
    return (
      <code className="md-code" {...props}>
        {children}
      </code>
    );
  },
};

export function Markdown({
  text,
  highlightCode = false,
}: {
  text: string;
  highlightCode?: boolean;
}) {
  return (
    <ReactMarkdown
      remarkPlugins={[remarkGfm]}
      components={highlightCode ? HIGHLIGHT_COMPONENTS : COMPONENTS}
    >
      {text}
    </ReactMarkdown>
  );
}
