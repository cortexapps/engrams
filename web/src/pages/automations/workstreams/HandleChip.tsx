import { parseHandle } from "./handles";

export function HandleChip({ handle }: { handle: string }) {
  const parsed = parseHandle(handle);
  const content = (
    <>
      <span className="font-mono text-2xs text-muted-foreground">{parsed.provider}</span>
      <span>{parsed.label}</span>
      {parsed.href && <span aria-hidden>↗</span>}
    </>
  );
  const className =
    "inline-flex items-center gap-1.5 rounded-sm border bg-card px-2 py-0.5 text-xs";

  return parsed.href ? (
    <a className={className} href={parsed.href} target="_blank" rel="noreferrer">
      {content}
    </a>
  ) : (
    <span className={className}>{content}</span>
  );
}
