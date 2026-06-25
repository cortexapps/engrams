/**
 * ProviderTile — the connector identity mark (Integrations & Profiles redesign).
 *
 * A square brand tile: a 1–2 char monogram on a brand tint, always rendered.
 * An optional logo (uploaded → served at `icon.logo`, or a pre-bundled built-in)
 * layers on top and falls back to the monogram on load error — so the tile is
 * never broken and stays crisp at 16px. Used everywhere a provider renders:
 * marketplace, profile editor, the Launch receipt, and in-session events.
 */

import { useState } from "react";

import { cn } from "@/lib/utils";

export interface ProviderTileProps {
  mono: string;
  color: string;
  /** Logo URL (uploaded serve URL or bundled asset). Falls back to the monogram. */
  logo?: string;
  /** Accessible label (the provider/display name). */
  name?: string;
  /** Pixel size of the square tile. */
  size?: number;
  className?: string;
}

export function ProviderTile({ mono, color, logo, name, size = 32, className }: ProviderTileProps) {
  const [logoFailed, setLogoFailed] = useState(false);
  const showLogo = Boolean(logo) && !logoFailed;

  return (
    <span
      role="img"
      aria-label={name ?? mono}
      className={cn(
        "relative inline-flex shrink-0 select-none items-center justify-center overflow-hidden font-mono font-bold text-white",
        className,
      )}
      style={{
        width: size,
        height: size,
        background: color,
        fontSize: Math.round(size * 0.34),
        borderRadius: size <= 18 ? "var(--radius-sm)" : "var(--radius-md)",
        boxShadow: "inset 0 0 0 1px color-mix(in oklch, black 18%, transparent)",
      }}
    >
      {/* The monogram is the base layer AND the fallback: hidden while a logo
          renders (else a transparent logo lets the letters ghost through), shown
          again if the logo fails to load. */}
      {!showLogo && mono}
      {showLogo && (
        <img
          src={logo}
          alt=""
          aria-hidden
          onError={() => setLogoFailed(true)}
          className="absolute inset-0 h-full w-full object-contain"
          style={{ padding: Math.round(size * 0.16) }}
        />
      )}
    </span>
  );
}
