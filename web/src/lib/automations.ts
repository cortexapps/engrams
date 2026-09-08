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

/** THE status → instrument tone mapping, shared by every surface that shows a
 * run or step status (list rows, runs tab, run page, step drawer) so they
 * cannot drift. Covers both vocabularies: run statuses (`completed`,
 * `filtered`, …) and step statuses (`succeeded`, `skipped`). `filtered`,
 * `superseded`, `halted`, and a skipped step are neutral outcomes (ended on
 * purpose), not failures; `active` covers everything still moving. Lime is
 * the accent, never a status (web/DESIGN.md). */
export function runStatusTone(status: string): RunTone {
  switch (status) {
    case "completed":
    case "succeeded":
      return "nominal";
    case "failed":
    case "deadline":
      return "critical";
    case "filtered":
    case "superseded":
    case "halted":
    case "skipped":
      return "caution";
    case "pending":
    case "running":
    case "waiting":
      return "active";
    default:
      return "muted";
  }
}

export function runStatusLabel(status: string): string {
  return automationStatusLabel(status);
}
