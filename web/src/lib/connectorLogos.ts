/**
 * Pre-bundled official brand icons for the built-in connectors.
 *
 * The connector JSON deliberately carries only a monogram + brand color (the
 * logo is not part of the connector config). An admin CAN upload a logo per
 * provider (→ `icon.logo` serve URL), but the built-ins ship their official
 * mark here so the marketplace/profile/launch surfaces look right out of the
 * box with no upload and no DB row.
 *
 * Each asset is a white-filled monochrome SVG (Simple Icons) that layers over
 * the provider's brand-color tile in {@link ProviderTile} — matching the white
 * monogram it replaces, and falling back to that monogram on load error. The
 * filename is the engrams provider id (e.g. `new_relic.svg`).
 *
 * Vite inlines the glob at build time, so adding `web/src/assets/connector-logos/
 * <provider>.svg` is all it takes to give a new connector an official icon.
 */

const modules = import.meta.glob<string>("../assets/connector-logos/*.svg", {
  eager: true,
  query: "?url",
  import: "default",
});

const byProvider: Record<string, string> = {};
for (const [path, url] of Object.entries(modules)) {
  const provider = path
    .split("/")
    .pop()!
    .replace(/\.svg$/, "");
  byProvider[provider] = url;
}

/** The bundled official-logo URL for a built-in provider, or undefined (→ fall
 * through to an uploaded `icon.logo` overlay, else the monogram). Built-ins ship
 * their canonical mark, so this takes precedence over an upload for that slug. */
export function builtinLogo(provider: string): string | undefined {
  return byProvider[provider];
}
