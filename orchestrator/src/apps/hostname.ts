/**
 * Session-app hostnames (ADR 0118).
 *
 * An app's public address is `<app-name>-<session-slug>.<previewBaseDomain>`.
 * The session draws ONE random `adjective-adjective-noun` slug at create; every
 * app in that session shares it, so N apps cost one random draw and produce
 * readable, related names (`web-tidy-swift-otters`, `api-tidy-swift-otters`).
 *
 * Why one label and not `<app>.<session>.<domain>`: the deployment's wildcard
 * certificate covers exactly one label under the base domain (`*.preview.…`),
 * and Google-managed certs cannot issue a nested wildcard. So the app name and
 * the session slug are joined with `-` into a single DNS label.
 *
 * The composed label is the routing key and is NEVER parsed back into its
 * parts — the edge looks the whole label up in `session_app`. That is why a
 * hyphen is a safe joiner even though app names may themselves contain hyphens.
 *
 * The label still does not encode the port (ADR 0064's property, kept), so it
 * cannot be used to scan a session's other ports. It is not a secret either:
 * authorization is the wall (ADR 0118), the name is only an address.
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
  // crypto-grade selection: a session slug is drawn once and shared by every
  // app in the session, so the predictability of Math.random() would make a
  // whole session's addresses guessable at once.
  const idx = crypto.getRandomValues(new Uint32Array(1))[0]! % list.length;
  return list[idx]!;
}

/** A fresh per-session `adjective-adjective-noun` slug. */
export function generateSessionSlug(): string {
  return `${pick(ADJECTIVES)}-${pick(ADJECTIVES)}-${pick(NOUNS)}`;
}

/** Longest DNS label a hostname may carry (RFC 1035). */
export const MAX_HOST_LABEL_LENGTH = 63;

/** Longest app name we accept, leaving room for `-<session-slug>` in one label. */
export const MAX_APP_NAME_LENGTH = 24;

/** A DNS-label-safe token: lowercase alphanumeric with interior hyphens. */
const LABEL_RE = /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/;

/**
 * True if `label` is shaped like a hostname label we would mint. Used by the
 * edge to fast-reject junk Host headers before touching the database.
 */
export function isValidHostLabel(label: string): boolean {
  return label.length <= MAX_HOST_LABEL_LENGTH && LABEL_RE.test(label);
}

/**
 * True if `name` is usable as the app half of a hostname label. Stricter than
 * `isValidHostLabel` only in length, so `<name>-<slug>` always fits in 63.
 */
export function isValidAppName(name: string): boolean {
  return name.length > 0 && name.length <= MAX_APP_NAME_LENGTH && LABEL_RE.test(name);
}

/**
 * The name an ad-hoc exposure gets when the caller supplies only a port — an
 * ad-hoc exposure IS an app, so there is one model rather than two.
 */
export function defaultAppName(port: number): string {
  return `port-${port}`;
}

/** Compose the routing key for one app. Callers must have validated `name`. */
export function appHostLabel(name: string, sessionSlug: string): string {
  return `${name}-${sessionSlug}`;
}

/** Local-dev preview domains are served over plain http; everything else https. */
export function schemeFor(domain: string): "http" | "https" {
  return /(localhost|127\.0\.0\.1|lvh\.me|localtest\.me)/.test(domain) ? "http" : "https";
}

/** The full public address of an app, e.g. `https://api-tidy-swift-otters.preview.x.io`. */
export function appUrl(hostLabel: string, baseDomain: string): string {
  return `${schemeFor(baseDomain)}://${hostLabel}.${baseDomain}`;
}
