/** DraftAutomation (Builder v2): a disabled automation plus the sandboxed
 * session that drafts it from a plain-English prompt.
 *
 * Deliberately minimal — the versions ARE the draft. The agent's proposals
 * land through `AutomationStore.saveVersion` on this ordinary (disabled)
 * automation; the human can hand-edit the same automation in the Builder
 * while the agent works, and the propose tool's `expected_version` fence
 * keeps the two honest. No document substrate, no phases, no status machine:
 * one nullable `draft_session_id` column is the whole data model, and it is
 * also the draft tools' authorization (a tool call may only write to the
 * automation bound to its own session).
 *
 * Idempotency follows the specs/create.ts ledger shape, trimmed: every id is
 * derived from (org, idempotency key), the automation row's primary key is
 * the ledger (a fixed-id insert conflicts on replay), and a reused key with
 * different arguments is refused via the request hash recorded on the task's
 * `source`.
 */

import { createHash } from "node:crypto";
import { Code, ConnectError } from "@connectrpc/connect";

import type { AutomationRow, AutomationStore, CreateAutomationInput } from "../db/automations.ts";
import { truncatePrompt } from "../rpc/task-create.ts";
import type { AutomationDefinition } from "./engine/definition.ts";
import { isUniqueViolation } from "../db/pg-errors.ts";

export const AUTOMATION_DRAFT_TASK_TYPE = "automation_draft";

/** Shown when the prompt yields no usable title. */
export const EMPTY_DRAFT_NAME = "Untitled automation";

/** The blank definition a draft starts from: a manual trigger and no
 * blocks — the agent's first proposal replaces it. */
export function emptyDraftDefinition(): AutomationDefinition {
  return {
    engine: 1,
    trigger: { kind: "manual" },
    blocks: [],
    inputsSchema: [],
    settings: { endSessionsOnFinish: false },
  };
}

export interface DraftSessionInput {
  taskId: string;
  sessionId: string;
  ownerUserId: string;
  ownerIsServiceAccount?: boolean;
  profileId: string;
  harness?: string;
  model?: string;
  modelRouter?: string;
  effort?: string;
  harnessMode?: string;
  requestHash: string;
  title: string;
  prompt: string;
  automationId: string;
}

export interface DraftAutomationRequest {
  orgId: string;
  ownerUserId: string;
  ownerIsServiceAccount?: boolean;
  idempotencyKey: string;
  prompt: string;
  profileId: string;
  harness?: string;
  model?: string;
  modelRouter?: string;
  effort?: string;
  harnessMode?: string;
}

export interface DraftAutomationDeps {
  store: Pick<AutomationStore, "create" | "get" | "setDraftSession" | "archive">;
  /** Owns the ordinary create path (`createTaskWithSession` with fixed ids
   * and task type "automation_draft"). */
  startSession: (input: DraftSessionInput) => Promise<void>;
}

export interface DraftAutomationResult {
  automationId: string;
  sessionId: string;
  created: boolean;
}

export async function draftAutomation(
  deps: DraftAutomationDeps,
  request: DraftAutomationRequest,
): Promise<DraftAutomationResult> {
  if (request.prompt.trim() === "") {
    throw new ConnectError("prompt is required", Code.InvalidArgument);
  }
  // Ids derive from (org, key, request hash): a byte-identical retry reaches
  // the same automation/session, while the same key with DIFFERENT arguments
  // simply derives a fresh draft — the web's signature-keyed idempotency
  // (NewSpecPage pattern) regenerates the key on edit anyway, so a refusal
  // path buys nothing here.
  const requestHash = draftRequestHash(request);
  const automationId = derivedUuid("automation-draft", request.orgId, request.idempotencyKey, requestHash);
  const taskId = derivedUuid("automation-draft-task", request.orgId, request.idempotencyKey, requestHash);
  const sessionId = derivedUuid("automation-draft-session", request.orgId, request.idempotencyKey, requestHash);
  const title = truncatePrompt(request.prompt) ?? EMPTY_DRAFT_NAME;

  const input: CreateAutomationInput = {
    id: automationId,
    name: title,
    description: "",
    enabled: false,
    definition: emptyDraftDefinition(),
    nextFireAt: null,
  };
  let row: AutomationRow;
  try {
    row = await deps.store.create(input, request.ownerUserId);
  } catch (error) {
    if (!isUniqueViolation(error)) throw error;
    return replayed(deps, automationId, sessionId);
  }
  await deps.store.setDraftSession(automationId, sessionId);

  try {
    await deps.startSession({
      taskId,
      sessionId,
      ownerUserId: request.ownerUserId,
      ...(request.ownerIsServiceAccount ? { ownerIsServiceAccount: true } : {}),
      profileId: request.profileId,
      ...(request.harness !== undefined ? { harness: request.harness } : {}),
      ...(request.model !== undefined ? { model: request.model } : {}),
      ...(request.modelRouter !== undefined ? { modelRouter: request.modelRouter } : {}),
      ...(request.effort !== undefined ? { effort: request.effort } : {}),
      ...(request.harnessMode !== undefined ? { harnessMode: request.harnessMode } : {}),
      requestHash,
      title: row.name,
      prompt: request.prompt,
      automationId,
    });
  } catch (error) {
    // Release the reservation: clear the binding FIRST (so a replay can
    // never report created:false against a draft whose session never
    // booted), then archive. A failed release never masks the original
    // error.
    try {
      await deps.store.setDraftSession(automationId, null);
      await deps.store.archive(automationId);
    } catch {
      // The row stays as a disabled draft; the archived/binding guards in
      // replayed() still refuse it.
    }
    throw error;
  }

  return { automationId, sessionId, created: true };
}

async function replayed(
  deps: DraftAutomationDeps,
  automationId: string,
  sessionId: string,
): Promise<DraftAutomationResult> {
  const existing = await deps.store.get(automationId);
  if (!existing) {
    throw new ConnectError("the reserved automation disappeared during creation", Code.Aborted);
  }
  if (existing.archivedAt !== null || existing.draftSessionId !== sessionId) {
    // The row exists but is unusable: its session never bound (a crash
    // between create and setDraftSession) or a failed session boot released
    // it (binding cleared + archived). Refuse rather than hand back a dead
    // draft; the caller retries with a fresh key.
    throw new ConnectError("the draft's session is not bound; retry with a new key", Code.Aborted);
  }
  return { automationId, sessionId, created: false };
}

export function draftRequestHash(request: DraftAutomationRequest): string {
  return createHash("sha256")
    .update(
      JSON.stringify({
        orgId: request.orgId,
        ownerUserId: request.ownerUserId,
        profileId: request.profileId,
        prompt: request.prompt,
        harness: request.harness ?? null,
        model: request.model ?? null,
        modelRouter: request.modelRouter ?? null,
        effort: request.effort ?? null,
        harnessMode: request.harnessMode ?? null,
      }),
    )
    .digest("hex");
}

/** A version-5-shaped UUID over the given parts (the specs/create.ts
 * derivation, kept local — the two domains must stay independently
 * evolvable). Each part carries its length, so no two different part lists
 * produce the same input string. */
export function derivedUuid(...parts: readonly string[]): string {
  const digest = createHash("sha256")
    .update(parts.map((part) => `${part.length}:${part}`).join(" "))
    .digest();
  digest[6] = (digest[6]! & 0x0f) | 0x50;
  digest[8] = (digest[8]! & 0x3f) | 0x80;
  const hex = digest.subarray(0, 16).toString("hex");
  return [
    hex.slice(0, 8),
    hex.slice(8, 12),
    hex.slice(12, 16),
    hex.slice(16, 20),
    hex.slice(20, 32),
  ].join("-");
}
