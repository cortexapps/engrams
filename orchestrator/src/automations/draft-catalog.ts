/** What the drafting agent reads (Builder v2): projections of the block
 * registry, the connector event/action catalogs, and profiles, shaped for a
 * model rather than a UI.
 *
 * The block catalog is derived live from the registry (`z.toJSONSchema` on
 * each executor's config schema — the same bridge the tool manifest uses),
 * so a new block type reaches the agent without a prompt change. A golden
 * test snapshots the projection: a registry change must be a visible diff.
 */

import { z } from "zod";

import {
  ENGINE_VERSION,
  entrypointSchema,
  inputFieldSchema,
  MAX_BLOCKS,
  MAX_ENTRYPOINTS,
  settingsSchema,
  triggerSpecSchema,
} from "./engine/definition.ts";
import { getBlock, isSystemBlockType, listBlockTypes } from "./engine/blocks/registry.ts";
import { registerEngineBlocks } from "./engine/blocks/index.ts";
import { loadEventSample } from "../connectors/samples.ts";
import { loadRegistry, type CustomConnectorSource } from "../connectors/registry.ts";
import type { IntegrationConnectionStore } from "../db/integration-connections.ts";
import type { IntegrationEventStore } from "../db/integration-events.ts";

/** A long sample payload is truncated for the tool result; the agent can
 * still see the shape, which is what it needs. */
export const SAMPLE_JSON_MAX_CHARS = 4_000;

function jsonSchemaOf(schema: z.ZodType): unknown {
  try {
    return z.toJSONSchema(schema as never);
  } catch {
    // A schema that resists projection (z.lazy recursion) still has to be
    // representable; the agent falls back to the conventions in its prompt.
    return { description: "schema not projectable; follow the definition conventions" };
  }
}

/** The block + definition catalog. Pure: derived from code, no I/O. */
export function draftBlockCatalog(): Record<string, unknown> {
  registerEngineBlocks();
  const blocks = listBlockTypes()
    .filter((type) => !isSystemBlockType(type))
    .map((type) => {
      const executor = getBlock(type)!;
      return {
        type,
        config_schema: jsonSchemaOf(executor.configSchema as z.ZodType),
        ...(executor.outputs ? { outputs: executor.outputs } : {}),
        ...(type === "branch" ? { nests: "branch (children in then/else)" } : {}),
        ...(type === "loop" ? { nests: "loop (children in body)" } : {}),
      };
    });
  return {
    engine_version: ENGINE_VERSION,
    max_blocks: MAX_BLOCKS,
    max_entrypoints: MAX_ENTRYPOINTS,
    blocks,
    trigger_schema: jsonSchemaOf(triggerSpecSchema),
    settings_schema: jsonSchemaOf(settingsSchema),
    input_field_schema: jsonSchemaOf(inputFieldSchema),
    entrypoint_schema: jsonSchemaOf(entrypointSchema),
    notes: [
      "A definition is {engine, trigger, blocks, entrypoints?, inputsSchema, settings}.",
      "Block ids are unique across ALL entrypoints; ids match ^[a-z][a-z0-9_]*$.",
      "branch children live in then/else; loop children in body; nothing else nests.",
      'Prose fields render Liquid with ${{ }}; structured values use {"$ref": "steps.<id>.<output>"}.',
      "At most one cron trigger per automation; extra entrypoints take integration, cron, or manual triggers.",
    ],
  };
}

export interface DraftEventCatalogDeps {
  connectors: CustomConnectorSource;
  connections: Pick<IntegrationConnectionStore, "getDefault">;
  integrationEvents: Pick<IntegrationEventStore, "getLatest" | "listObservedEventKeys">;
  eventSample?: typeof loadEventSample;
}

function truncateSample(json: string): string {
  return json.length > SAMPLE_JSON_MAX_CHARS
    ? `${json.slice(0, SAMPLE_JSON_MAX_CHARS)}… (truncated)`
    : json;
}

/** Every provider with a webhook facet: its events (with the freshest real
 * sample, else the checked-in fixture) and whether a connection exists. */
export async function draftEventCatalog(deps: DraftEventCatalogDeps): Promise<unknown> {
  const sample = deps.eventSample ?? loadEventSample;
  const registry = await loadRegistry(deps.connectors);
  const providers = [];
  for (const [provider, connector] of registry) {
    const facet = connector.webhook;
    if (!facet) continue;
    const connection = await deps.connections.getDefault(provider);
    const observed = new Set(
      connection ? await deps.integrationEvents.listObservedEventKeys(connection.id) : [],
    );
    const events = [];
    for (const event of facet.events) {
      if (event.hidden === true) continue;
      let sampleJson = "";
      if (connection) {
        const latest = await deps.integrationEvents.getLatest(connection.id, event.key);
        if (latest) sampleJson = JSON.stringify(latest.payload);
      }
      if (sampleJson === "") {
        const fixture = sample(provider, event.key);
        if (fixture) sampleJson = JSON.stringify(fixture);
      }
      events.push({
        key: event.key,
        label: event.label,
        ...(event.description ? { description: event.description } : {}),
        observed: observed.has(event.key),
        ...(sampleJson !== "" ? { sample_json: truncateSample(sampleJson) } : {}),
      });
    }
    providers.push({
      provider,
      connected: connection !== null,
      ...(connection ? { connection_id: connection.id } : {}),
      ...(facet.scope ? { scope: { key: facet.scope.key, label: facet.scope.label } } : {}),
      event_aliases: facet.aliases,
      events,
    });
  }
  return { providers };
}

/** Every provider's actions (for integration_action blocks). */
export async function draftActionCatalog(connectors: CustomConnectorSource): Promise<unknown> {
  const registry = await loadRegistry(connectors);
  const providers = [];
  for (const [provider, connector] of registry) {
    const actions = connector.actions ?? [];
    if (actions.length === 0) continue;
    providers.push({
      provider,
      actions: actions.map((action) => ({
        id: action.id,
        label: action.label,
        ...(action.description ? { description: action.description } : {}),
        input_schema: action.inputSchema,
      })),
    });
  }
  return { providers };
}
