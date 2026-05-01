// `── run 9a7c · prompt: "fix the failing test" ──`
//
// Drawn between runs so the transcript reads as a series of bounded
// scenes rather than an unbroken scroll.

export function RunBoundary({
  runId,
  prompt,
}: {
  runId: string;
  prompt: string | null;
}) {
  const short = runId.slice(0, 6);
  return (
    <div
      className="my-8 flex items-center gap-3 font-mono smallcaps text-[0.66rem]"
      style={{ color: 'var(--color-ink-quiet)' }}
    >
      <hr className="flex-1" />
      <span>run {short}</span>
      {prompt && (
        <span
          className="font-display italic max-w-[40ch] truncate"
          style={{
            fontVariantCaps: 'normal',
            letterSpacing: 0,
            color: 'var(--color-ink-faded)',
          }}
          title={prompt}
        >
          “{prompt}”
        </span>
      )}
      <hr className="flex-1" />
    </div>
  );
}

export function IdleMarker() {
  return (
    <div
      className="my-6 text-center font-display italic"
      style={{ color: 'var(--color-verdigris)' }}
    >
      <div className="glyph text-[1.1rem]">◌</div>
      <div className="font-mono smallcaps text-[0.66rem] mt-1">
        awaiting prompt
      </div>
    </div>
  );
}
