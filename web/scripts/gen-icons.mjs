#!/usr/bin/env node
// Regenerates the app icon family from one description of the mark
// (design handoff §8): a bottle-green tile with a carbon twill, the engram
// trace in sage, the amber entry ring, the lime terminal dot. Writes the SVG
// plus the PNG/ICO rasters into web/public and mirrors them into the site.
//
//   node scripts/gen-icons.mjs
//
// Needs `rsvg-convert` (librsvg) on PATH for the rasters.

import { execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const web = join(here, "..");
const repo = join(web, "..");

// Colours (sRGB hex, since an icon file cannot read the app's tokens).
const GRADIENT_TOP = "#2f4e4c"; // oklch(0.36 0.052 190)
const GRADIENT_BOTTOM = "#213b3a"; // oklch(0.29 0.048 190)
const SAGE = "#e6ebd8"; // --sidebar-foreground
const AMBER = "#e0913d"; // the entry ring on the cover
const LIME = "#bddf42"; // --sidebar-primary

/** The trace, inset 16% for the tile. */
const TILE_TRACE = "M16 67 L33 50 L50 67 L67 33 L84 50";

function tile({ size, stroke, ring, dot }) {
  const radius = Math.round(100 * 0.23);
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100" width="${size}" height="${size}">
  <defs>
    <linearGradient id="g" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="${GRADIENT_TOP}"/>
      <stop offset="1" stop-color="${GRADIENT_BOTTOM}"/>
    </linearGradient>
    <pattern id="twill" width="8" height="8" patternUnits="userSpaceOnUse" patternTransform="rotate(45)">
      <rect width="4" height="8" fill="rgba(0,0,0,0.10)"/>
    </pattern>
    <clipPath id="tile"><rect width="100" height="100" rx="${radius}"/></clipPath>
  </defs>
  <g clip-path="url(#tile)">
    <rect width="100" height="100" fill="url(#g)"/>
    <rect width="100" height="100" fill="url(#twill)"/>
    <rect x="0" y="0.5" width="100" height="1" fill="rgba(255,255,255,0.14)"/>
    <rect x="0" y="98.5" width="100" height="1" fill="rgba(0,0,0,0.25)"/>
  </g>
  <path d="${TILE_TRACE}" fill="none" stroke="${SAGE}" stroke-width="${stroke}" stroke-linecap="round" stroke-linejoin="round"/>
  ${ring ? `<circle cx="16" cy="67" r="6.5" fill="url(#g)" stroke="${AMBER}" stroke-width="5"/>` : ""}
  <circle cx="84" cy="50" r="${dot}" fill="${LIME}"/>
</svg>
`;
}

// ≥ 32px: the full mark. 16px: heavier stroke, bigger dot, no ring.
const large = tile({ size: 100, stroke: 9, ring: true, dot: 7.5 });
const small = tile({ size: 16, stroke: 13, ring: false, dot: 11 });

const tmp = mkdtempSync(join(tmpdir(), "engram-icons-"));
const largeSvg = join(tmp, "large.svg");
const smallSvg = join(tmp, "small.svg");
writeFileSync(largeSvg, large);
writeFileSync(smallSvg, small);

function raster(svg, px) {
  return execFileSync("rsvg-convert", ["-w", String(px), "-h", String(px), svg]);
}

const png32 = raster(largeSvg, 32);
const png16 = raster(smallSvg, 16);
const png180 = raster(largeSvg, 180);

/** A one-image ICO whose entry is a PNG — what every current browser reads. */
function ico(png, px) {
  const header = Buffer.alloc(6 + 16);
  header.writeUInt16LE(0, 0); // reserved
  header.writeUInt16LE(1, 2); // type: icon
  header.writeUInt16LE(1, 4); // one image
  header.writeUInt8(px, 6);
  header.writeUInt8(px, 7);
  header.writeUInt8(0, 8); // palette
  header.writeUInt8(0, 9); // reserved
  header.writeUInt16LE(1, 10); // planes
  header.writeUInt16LE(32, 12); // bpp
  header.writeUInt32LE(png.length, 14);
  header.writeUInt32LE(6 + 16, 18);
  return Buffer.concat([header, png]);
}

const outputs = [
  ["engram-trace.svg", Buffer.from(large)],
  ["favicon-32.png", png32],
  ["favicon-16.png", png16],
  ["apple-touch-icon.png", png180],
  ["favicon.ico", ico(png32, 32)],
];

for (const dir of [join(web, "public"), join(repo, "site", "public")]) {
  mkdirSync(dir, { recursive: true });
  for (const [name, bytes] of outputs) writeFileSync(join(dir, name), bytes);
}
// Starlight's logo is the same tile.
writeFileSync(join(repo, "site", "src", "assets", "engram-trace.svg"), large);

console.log(
  outputs.map(([name, bytes]) => `${name} ${bytes.length}B`).join("\n"),
  "\n→ web/public, site/public, site/src/assets",
);
void readFileSync;
