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

export type AutomationField =
  | "name"
  | "schedule"
  | "timezone"
  | "registration"
  | "events"
  | "profile"
  | "promptTemplate"
  | "titleTemplate"
  | "form";

/** Map server InvalidArgument prose to the control that can resolve it. */
export function automationErrorField(message: string): AutomationField {
  const text = message.toLowerCase();
  if (text.includes("timezone")) return "timezone";
  if (text.includes("cron") || text.includes("schedule")) return "schedule";
  if (text.includes("registration")) return "registration";
  if (text.includes("event")) return "events";
  if (text.includes("profile")) return "profile";
  if (text.includes("title")) return "titleTemplate";
  if (text.includes("template") || text.includes("prompt")) return "promptTemplate";
  if (text.includes("name")) return "name";
  return "form";
}

export interface RawVariable {
  path: string;
  depth: number;
}

/** Flatten the latest redacted payload into click-to-insert event.raw leaves. */
export function rawVariables(payloadJson: string | undefined): RawVariable[] {
  if (!payloadJson) return [];
  try {
    const payload: unknown = JSON.parse(payloadJson);
    const result: RawVariable[] = [];
    const visit = (value: unknown, path: string, depth: number) => {
      if (result.length >= 150) return;
      if (Array.isArray(value)) {
        if (value.length === 0) result.push({ path, depth });
        else visit(value[0], path ? `${path}.0` : "0", depth + 1);
        return;
      }
      if (isRecord(value)) {
        const entries = Object.entries(value);
        if (entries.length === 0 && path) result.push({ path, depth });
        for (const [key, child] of entries) {
          visit(child, path ? `${path}.${key}` : key, depth + 1);
        }
        return;
      }
      if (path) result.push({ path, depth });
    };
    visit(payload, "", 0);
    return result;
  } catch {
    return [];
  }
}

export function automationStatusLabel(status: string): string {
  return status.replaceAll("_", " ");
}
