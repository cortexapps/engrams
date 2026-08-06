import { EngramMark } from "./EngramMark";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

// The shared connection-state surface for the live panes (shell + browser).
// Both used to push a one-line italic banner above the canvas, shifting it on
// every state change. Instead this floats one calm overlay centred over the
// canvas box: the animated engram trace while we're bringing the connection up,
// and a quiet message + Reconnect when it drops. `connected` renders nothing,
// so the live canvas owns the box uninterrupted.

export type PanePhase = "loading" | "connecting" | "connected" | "closed" | "error";

export interface PaneStatusProps {
  phase: PanePhase;
  /** Shown while the connection is coming up (loading / connecting). */
  caption: string;
  /** The detail line for a dropped/failed connection. */
  message?: string | null;
  /** Force a fresh connection attempt (remounts the underlying client). */
  onReconnect?: () => void;
}

export function PaneStatus({ phase, caption, message, onReconnect }: PaneStatusProps) {
  if (phase === "connected") return null;

  const busy = phase === "loading" || phase === "connecting";

  return (
    <div
      role="status"
      aria-live="polite"
      className="absolute inset-0 z-10 grid place-items-center bg-card/95 px-6 text-center backdrop-blur-[2px]"
    >
      {busy ? (
        <div className="flex flex-col items-center gap-3.5">
          <EngramMark size={56} mode="loop" title={caption} />
          <p className="text-[0.92rem] text-muted-foreground italic">{caption}</p>
        </div>
      ) : (
        <div className="flex max-w-xs flex-col items-center gap-3">
          <EngramMark size={44} mode="static" />
          <p
            className={cn(
              "text-pretty",
              phase === "error"
                ? "font-mono text-[0.78rem] text-destructive"
                : "text-[0.92rem] text-muted-foreground italic",
            )}
          >
            {message ?? (phase === "error" ? "unavailable" : "connection closed")}
          </p>
          {onReconnect && (
            <Button size="sm" variant="outline" onClick={onReconnect} className="mt-1">
              Reconnect
            </Button>
          )}
        </div>
      )}
    </div>
  );
}
