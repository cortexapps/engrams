/**
 * Vanity-slug generation for port exposures (ADR 0064).
 *
 * A slug is the routing key in `<slug>.preview.<domain>` — an opaque,
 * auto-minted "adjective-adjective-noun" triple (e.g. `jumping-fat-kittens`).
 * Auto-generated (never user-chosen), so there's no name-reservation or
 * collision UX; and it deliberately encodes neither the session nor the port,
 * so a slug can't be used to enumerate a session's other ports.
 *
 * Every word is a lowercase `[a-z]+` token and the triple is joined by `-`, so
 * the result is always a valid DNS label (≤63 chars, no leading/trailing
 * hyphen). The store retries on the (rare) primary-key collision, so the word
 * lists only need to make collisions unlikely, not impossible — ~50×50×60 ≈
 * 150k combinations is plenty for the handful of live previews per session.
 */

const ADJECTIVES = [
  "jumping", "fat", "happy", "sleepy", "brave", "clever", "shiny", "quiet",
  "lucky", "fuzzy", "gentle", "swift", "bold", "calm", "eager", "fancy",
  "jolly", "kind", "lively", "merry", "nimble", "proud", "silly", "witty",
  "zesty", "breezy", "cozy", "dapper", "feisty", "glossy", "humble", "ivory",
  "jazzy", "keen", "lush", "mellow", "noble", "peppy", "quirky", "rosy",
  "snug", "tidy", "upbeat", "vivid", "wavy", "amber", "azure", "crimson",
  "golden", "teal",
];

const NOUNS = [
  "kittens", "otters", "pandas", "foxes", "owls", "geckos", "llamas", "bunnies",
  "puffins", "narwhals", "badgers", "ferrets", "hedgehogs", "wombats", "lemurs",
  "marmots", "raccoons", "beavers", "dolphins", "walruses", "penguins", "koalas",
  "meerkats", "platypus", "axolotls", "mantises", "newts", "toucans", "quokkas",
  "tapirs", "ibex", "yaks", "moose", "bison", "herons", "cranes", "finches",
  "magpies", "ravens", "sparrows", "comets", "nebulas", "pulsars", "quasars",
  "geysers", "glaciers", "canyons", "meadows", "harbors", "lagoons",
];

function pick<T>(list: readonly T[]): T {
  // crypto-grade selection — slugs double as unguessable handles, so avoid the
  // predictability of Math.random().
  const idx = crypto.getRandomValues(new Uint32Array(1))[0]! % list.length;
  return list[idx]!;
}

/** Generate a fresh `adjective-adjective-noun` slug. */
export function generateSlug(): string {
  return `${pick(ADJECTIVES)}-${pick(ADJECTIVES)}-${pick(NOUNS)}`;
}

/** A DNS-label-safe slug: 1..=63 chars, lowercase alphanumeric + interior hyphens. */
const SLUG_RE = /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/;

/** True if `s` is shaped like a slug we'd mint (used to fast-reject junk hostnames). */
export function isValidSlug(s: string): boolean {
  return SLUG_RE.test(s);
}
