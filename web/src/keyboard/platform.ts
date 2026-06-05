// Which glyphs to print for the modifier keys. macOS prints the familiar
// ⌘/⌥/⇧ symbols; everywhere else we spell them out so the cheatsheet stays
// honest on Windows/Linux. Computed once — the platform doesn't change
// mid-session. `navigator.platform` is deprecated but still the most reliable
// mac tell; we fall back to the UA string.
const ua = typeof navigator !== "undefined" ? navigator.userAgent : ""
const platform =
  typeof navigator !== "undefined"
    ? (navigator.platform ?? "")
    : ""

export const IS_MAC = /mac/i.test(platform) || /mac/i.test(ua)

/** The command/control key glyph for hints (⌘ on mac, "Ctrl" elsewhere). */
export const MOD_LABEL = IS_MAC ? "⌘" : "Ctrl"
/** The option/alt key glyph — the session-jump layer's modifier. */
export const ALT_LABEL = IS_MAC ? "⌥" : "Alt"
