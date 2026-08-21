/** Checked-in event sample fixtures (ADR 0119 D5).
 *
 * One redacted, trimmed real payload per declared (non-hidden) catalog event,
 * at `samples/<provider>/<event_key>.json`. The catalog RPC serves the newest
 * ledger row when one exists and falls back to these. Loaded lazily; the
 * registry loader never touches them (they live in a subdirectory precisely
 * so `loadFromDisk`'s `*.json` scan cannot parse them as connectors).
 */

import { readFileSync } from "node:fs";
import { join } from "node:path";

import { redactWebhookPayload } from "../automations/webhook.ts";

const PROVIDER_RE = /^[a-z0-9][a-z0-9_-]*$/;
const EVENT_KEY_RE = /^[a-z0-9_-]+(?:\.[a-z0-9_-]+)*$/;

const cache = new Map<string, Record<string, unknown> | null>();

/** Load one event sample, redacted defensively (a fixture should already be
 * clean; redaction makes that a property, not a review hope). Null when the
 * provider/key has no fixture. */
export function loadEventSample(provider: string, eventKey: string): Record<string, unknown> | null {
  if (!PROVIDER_RE.test(provider) || !EVENT_KEY_RE.test(eventKey)) return null;
  const cacheKey = `${provider}:${eventKey}`;
  const cached = cache.get(cacheKey);
  if (cached !== undefined) return cached;
  let sample: Record<string, unknown> | null;
  try {
    const raw = JSON.parse(
      readFileSync(join(import.meta.dir, "samples", provider, `${eventKey}.json`), "utf8"),
    ) as unknown;
    sample =
      typeof raw === "object" && raw !== null && !Array.isArray(raw)
        ? redactWebhookPayload(raw as Record<string, unknown>)
        : null;
  } catch {
    sample = null;
  }
  cache.set(cacheKey, sample);
  return sample;
}
