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

// ---------------------------------------------------------------------------
// Single-block definition adapter (ADR 0119 phase 3.1). The legacy editor
// still authors "trigger + one create_session block"; these helpers convert
// that draft to/from the v2 `definition_json` the RPC speaks. The phase-3 UI
// replaces this page and these helpers go with it.
// ---------------------------------------------------------------------------

export const SINGLE_BLOCK_ID = "create_session";

export interface SingleBlockDraft {
  triggerKind: "cron" | "webhook";
  schedule: string;
  timezone: string;
  registrationId: string;
  events: string[];
  profileId: string;
  promptTemplate: string;
  titleTemplate: string;
  includeEventContext: boolean;
  harnessMode?: string;
  harness?: string;
  model?: string;
  modelRouter?: string;
  effort?: string;
}

interface DefinitionShape {
  engine: 1;
  trigger:
    | { kind: "cron"; schedule: string; timezone: string }
    | { kind: "webhook"; registrationId: string; events: string[] }
    | { kind: string; [k: string]: unknown };
  blocks: Array<{ id: string; type: string; config: Record<string, unknown> }>;
  inputsSchema: unknown[];
  settings: { endSessionsOnFinish: boolean };
}

export function definitionFromDraft(draft: SingleBlockDraft): string {
  const config: Record<string, unknown> = {
    profileId: draft.profileId,
    promptTemplate: draft.promptTemplate,
    includeEventContext: draft.includeEventContext,
  };
  if (draft.titleTemplate.trim()) config.titleTemplate = draft.titleTemplate;
  if (draft.harnessMode) config.harnessMode = draft.harnessMode;
  if (draft.harness) config.harness = draft.harness;
  if (draft.model) config.model = draft.model;
  if (draft.modelRouter !== undefined) config.modelRouter = draft.modelRouter;
  if (draft.effort) config.effort = draft.effort;
  const definition: DefinitionShape = {
    engine: 1,
    trigger:
      draft.triggerKind === "cron"
        ? { kind: "cron", schedule: draft.schedule, timezone: draft.timezone }
        : { kind: "webhook", registrationId: draft.registrationId, events: draft.events },
    blocks: [{ id: SINGLE_BLOCK_ID, type: "create_session", config }],
    inputsSchema: [],
    settings: { endSessionsOnFinish: false },
  };
  return JSON.stringify(definition);
}

/** Read a stored definition back into the legacy draft. Null when the
 * automation is not a single create_session block (built-ins, multi-block
 * graphs) — the legacy editor cannot edit those; it shows a notice. */
export function draftFromDefinition(definitionJson: string): SingleBlockDraft | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(definitionJson);
  } catch {
    return null;
  }
  if (!isRecord(parsed) || !Array.isArray(parsed.blocks) || parsed.blocks.length !== 1) return null;
  const block = parsed.blocks[0];
  if (!isRecord(block) || block.type !== "create_session" || !isRecord(block.config)) return null;
  const trigger = isRecord(parsed.trigger) ? parsed.trigger : null;
  if (!trigger || (trigger.kind !== "cron" && trigger.kind !== "webhook")) return null;
  const c = block.config;
  const str = (v: unknown) => (typeof v === "string" ? v : undefined);
  return {
    triggerKind: trigger.kind,
    schedule: str(trigger.schedule) ?? "",
    timezone: str(trigger.timezone) ?? "",
    registrationId: str(trigger.registrationId) ?? "",
    events: Array.isArray(trigger.events)
      ? trigger.events.filter((e): e is string => typeof e === "string")
      : [],
    profileId: str(c.profileId) ?? "",
    promptTemplate: str(c.promptTemplate) ?? "",
    titleTemplate: str(c.titleTemplate) ?? "",
    includeEventContext: c.includeEventContext === true,
    ...(str(c.harnessMode) !== undefined ? { harnessMode: str(c.harnessMode) } : {}),
    ...(str(c.harness) !== undefined ? { harness: str(c.harness) } : {}),
    ...(str(c.model) !== undefined ? { model: str(c.model) } : {}),
    ...(str(c.modelRouter) !== undefined ? { modelRouter: str(c.modelRouter) } : {}),
    ...(str(c.effort) !== undefined ? { effort: str(c.effort) } : {}),
  };
}

/** Lift the single block's rendered prompt/title out of a v2 TestRender
 * response (per-block rendered configs). */
export function singleBlockPreview(blocks: Array<{ blockId: string; renderedJson: string }>): {
  renderedPrompt?: string;
  renderedTitle?: string;
} {
  const block = blocks.find((b) => b.blockId === SINGLE_BLOCK_ID);
  if (!block) return {};
  try {
    const rendered: unknown = JSON.parse(block.renderedJson);
    if (!isRecord(rendered)) return {};
    return {
      ...(typeof rendered.promptTemplate === "string"
        ? { renderedPrompt: rendered.promptTemplate }
        : {}),
      ...(typeof rendered.titleTemplate === "string"
        ? { renderedTitle: rendered.titleTemplate }
        : {}),
    };
  } catch {
    return {};
  }
}

/** The run page's summary fields now live on step outputs; the legacy table
 * reads the create_session block's outputs for the prompt/session link. */
export function runStatusLabel(status: string): string {
  return automationStatusLabel(status);
}
