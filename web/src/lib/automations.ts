import type { Connector } from "@/gen/engram/app/v1/integration_pb";

/** Custom registrations verify with the generic scheme only (ADR 0119 D5);
 * provider events are integration triggers. */
export type VerificationScheme = "generic_hmac_sha256";

export interface WebhookConnectorHint {
  provider: string;
  name: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Read only the display fields the server already validated from connector
 * JSON. Providers that own an integration ingress (an `ingress` facet) are
 * excluded: their events are integration triggers, and the server rejects
 * them as custom-registration hints. */
export function webhookConnectorHints(connectors: Connector[]): WebhookConnectorHint[] {
  const hints: WebhookConnectorHint[] = [];
  for (const connector of connectors) {
    try {
      const config: unknown = JSON.parse(connector.configJson);
      if (!isRecord(config) || !isRecord(config.webhook)) continue;
      if (config.webhook.ingress !== undefined) continue;
      const display = isRecord(config.display) ? config.display : undefined;
      hints.push({
        provider: connector.provider,
        name: typeof display?.name === "string" ? display.name : connector.provider,
      });
    } catch {
      // The server validates connector JSON. Ignore a stale malformed row rather
      // than offering a provider that registration creation would reject.
    }
  }
  return hints.sort((a, b) => a.name.localeCompare(b.name));
}

export function automationStatusLabel(status: string): string {
  return status.replaceAll("_", " ");
}

export type RunTone = "nominal" | "caution" | "critical" | "muted" | "active";

/** Status → instrument tone for the list's status dot. `filtered`,
 * `superseded`, and `halted` are neutral outcomes (the run ended on purpose),
 * not failures; `active` covers everything still moving. */
export function runStatusTone(status: string): RunTone {
  switch (status) {
    case "completed":
      return "nominal";
    case "failed":
    case "deadline":
      return "critical";
    case "filtered":
    case "superseded":
    case "halted":
      return "caution";
    case "pending":
    case "running":
    case "waiting":
      return "active";
    default:
      return "muted";
  }
}

/** "3m ago" / "2h ago" / "5d ago"; falls back to the ISO date past a month. */
/** Past or future, symmetric: "4m ago" / "in 4m". Within a minute either
 * way reads as "just now" / "any moment"; beyond a month, the date. */
export function relativeTime(iso: string | undefined, now: Date = new Date()): string {
  if (!iso) return "never";
  const then = new Date(iso);
  if (Number.isNaN(then.getTime())) return iso;
  const delta = Math.round((now.getTime() - then.getTime()) / 1000);
  const future = delta < 0;
  const s = Math.abs(delta);
  if (s < 60) return future ? "any moment" : "just now";
  const unit = (n: number, suffix: string) => (future ? `in ${n}${suffix}` : `${n}${suffix} ago`);
  const m = Math.round(s / 60);
  if (m < 60) return unit(m, "m");
  const h = Math.round(m / 60);
  if (h < 24) return unit(h, "h");
  const d = Math.round(h / 24);
  if (d < 31) return unit(d, "d");
  return then.toISOString().slice(0, 10);
}

export function runStatusLabel(status: string): string {
  return automationStatusLabel(status);
}
