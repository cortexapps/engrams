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


/** The design idioms for workflows that span multiple events or days. The
 * block catalog teaches WHAT exists; this teaches HOW the engine is meant
 * to be composed. Static by design — review it when the engine's rules
 * change (a stale idiom here misleads every draft). */
export function draftPatterns(): Record<string, unknown> {
  return {
    core_model: [
      "Runs are SHORT and stateless. Waits inside a run cap at 24h and stuck runs are swept at 48h - never design a run to live for a day.",
      "A workflow that spans days = multiple ENTRYPOINTS (one per way in: an integration event, a cron tick, a manual kick) sharing the automation's inputs, settings, and state. Each run enters through one entrypoint, does one step of the lifecycle, and exits.",
      "A TEMPLATED workflow (the same lifecycle per project/customer/case) declares settings.instance and becomes one automation with many WORKSTREAMS - see the instances section; never duplicate an automation per project.",
      "Conversations (hours, every event carries the same key) are the ONE long-lived-run shape: concurrency policy join + trigger.continueOnly, like the Slack brain built-in.",
      "At most one cron trigger per automation.",
      "ONE template contract everywhere: every rendered string uses ${{ ... }} (Liquid) - session {template} refs included. Plain {{ ... }} NEVER renders (it flows through as a literal) and save refuses it; code block source is JS, not a template.",
    ],
    instances: [
      "settings.instance = {keyTemplate, inputs?, entrypoints?} turns the automation into a TEMPLATE: one open workstream per rendered key (e.g. keyTemplate 'project-${{ inputs.linear_project_id }}'), each with its OWN input snapshot, state, sessions, and cron ticks. The author makes ONE decision - the key; isolation and routing follow.",
      "Admission per entrypoint: admit 'open' (default - render the key, open or join), 'require' (join an open workstream or DROP - use for mid-lifecycle events that make no sense without a kickoff), 'handle_match' (route ONLY by accumulated external identifiers - use for reply/reaction/review entrypoints).",
      "Handles are automatic: slack.post_message binds the thread it posts into (and its own ts), a session that opens a PR binds github:<repo>#<n>. A Slack reply or a PR review then routes to the RIGHT workstream with zero routing blocks - do not hand-roll thread:<ts> state keys for routing any more.",
      "A workstream can own a whole Slack CHANNEL: claim_handle with 'slack:${{ steps.<post>.channel }}' (use the post block's OUTPUT channel - it is the channel ID; the input may be a #name). Then EVERY channel message - top-level messages included, which thread binding alone never sees - routes to the workstream, and the general brain stands down channel-wide (explicit @-mentions included: the workstream IS the channel's interface). A thread another workstream owns still wins over the channel (most specific open owner). Join first with the slack.join_channel action - message events only flow where the bot is a channel member (public channels; private ones need a human /invite).",
      "Claiming a handle another OPEN workstream holds is a hard error - never rebind live routing. A handle whose holder is CLOSED is taken over by the claim (outputs.reclaimed = true): channels outlive workstreams, so next quarter's workstream claims the same channel cleanly. When every workstream bound in a channel has closed, the brain resumes answering there.",
      "RESTRAINT is mandatory for a channel-bound flow: every channel message reaches the session, so its STANDING prompt (the create_session prompt, not the per-message relay) must state the response policy - respond only when directly addressed, asked a question it can answer, or the message requires project action; otherwise silently absorb anything relevant (update notes/state) and end the turn WITHOUT posting. An agent that chimes into human-to-human chatter gets the bot removed from the channel; silence is the default.",
      "Give the model the social signal cheaply: a channel-message relay template should carry whether the bot was @-mentioned ('${{ event.raw.event.text }}' contains the bot mention) and whether the message is in a thread the session started - both from the event payload, so the session does not have to guess who is being spoken to.",
      "Bind DEDICATED workstream channels only (a channel the project owns socially, e.g. #project-<name>) - never a shared space like #general: channel binding silences the general brain there and routes all traffic to one workstream, which is only right when the channel IS the project. For shared spaces, leave the brain in place (routing arbitration is a designed follow-up).",
      "State inside an instance-bound run is automatically scoped to the workstream: write plain keys ('plan', 'tickets') and two projects never collide. Concurrency keys scope the same way.",
      "Close the lifecycle with the instance_close block (a run closes only its own workstream); later events for it are dropped and audited. Kicking off the same key again starts a FRESH workstream.",
      "RunNow on an instanced automation takes instance_key + instance_inputs_json to open or join a workstream; the automation row's inputs are only defaults for new workstreams.",
    ],
    state: [
      "automation_state is the shared memory across entrypoints and runs: state key = entity (one JSON document per entity, e.g. ticket:ENG-123), and make the automation's concurrency keyTemplate render the SAME entity key with policy queue - then runs touching one entity serialize and get-then-set needs no locks. For per-project workflows prefer settings.instance (see instances) - it scopes state per workstream automatically.",
      "Actors that cannot hold the entity claim (a cron sweep over many entities) write with expectVersion (CAS); an ok:false result is a branchable output, and losing a race to a real per-entity run is usually the correct outcome.",
      "There is no lock block and no cross-block transaction on purpose; put facts that must change together in one document.",
    ],
    kept_sessions: [
      'Sessions are kept by default and outlive their runs. Record a long-lived session id in state (e.g. state["pm"].session_id).',
      "A later run reaches a kept session with a {template} session ref: resolving it ADOPTS the session (re-binds its event routing to this run) - allowed only within the same automation and only when the owning run finished, so adoption never steals from a live waiting run.",
      "Probe before you prompt: session_status is read-only, never re-binds, and reports found/status/idle_seconds/owner_run_live as VALUES a filter can branch on - the cheap look for a sweep entrypoint.",
      'For an agent inside a session to talk back to the run waiting on it, the session uses its signal_automation tool and the run waits with send_prompt waitFor {kind: "signal", name} or wait_session.',
    ],
    slack_clarification_thread: [
      "Post with integration_action slack.post_message (it accepts thread_ts and RETURNS the posted ts). In the same run, state_set key thread:<ts> value {session_id, ticket, ...}. Exit.",
      "A second entrypoint triggers on slack message in that channel; a filter keeps only events whose thread_ts has a state entry; the run reads the entry, adopts the bound session by id, send_prompts the human reply into it, and exits.",
      "The Slack app must be a member of the channel for message events to arrive.",
    ],
    pr_feedback_loop: [
      "Every session that opens a PR is auto-recorded in the pr_ref ledger. The lookup_pr_session block maps {repo, prNumber} to the authoring session - found:false is a value, so unrelated PRs just filter out.",
      'So: an entrypoint on pull_request_review.submitted / pull_request_review_comment.created / issue_comment.created -> lookup_pr_session -> filter on found -> send_prompt with session {template: "${{ steps.<lookup>.session_id }}"} delivers the feedback to the implementer. Adoption handles the routing.',
    ],
    delegation: [
      "A coordinator session can spawn and manage its own sub-sessions with the session-side coordination tools (spawn_session, send_session_message, read_session, wait_sessions) - good for a manager/worker shape.",
      "Automation-owned coordinators spawn on ORG authority: children inherit the parent's frozen launch policy and programmatic credentials — no human owner or ownerUserId needed.",
      "The engine-side alternative - the run creates sessions with create_session blocks - is more auditable in the run ledger; prefer it when the set of workers is decided by the workflow, and spawn_session when the coordinating agent decides dynamically.",
    ],
    cron_heartbeat: [
      "A multi-day lifecycle stays alive through a cron entrypoint: each tick reads state, probes sessions (session_status), adopts + prompts whatever is stuck or due, updates state, exits. The coordinator session learns of progress on the next tick - like a coworker checking their board.",
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
