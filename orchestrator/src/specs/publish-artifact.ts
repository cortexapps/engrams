/**
 * The two side effects of a publish that leave the orchestrator's own tables:
 * the artifact version (ADR 0114 D10 over ADR 0026) and the ticketize hand-off
 * (R36).
 *
 * The artifact registry has no bytes-in API — publish reads the bytes out of
 * the guest with `CreateArtifactFromPath`. So a publish stages the pinned
 * markdown in the sandbox first, exactly as the projection stages its render,
 * and then records the version. Both legs therefore need a live session, which
 * is why the scanner owns them and a failure is a retry rather than a lost
 * publish.
 *
 * Exactly-once comes from two given ids: the artifact id is minted with the
 * publish request, and the staging path is keyed by the pinned checkpoint. The
 * `WriteFile` primitive is create-only and accepts a retry with the same bytes
 * (ADR 0113), so the staging step is a no-op on replay, and the registry insert
 * at a known id makes a duplicate detectable instead of silent.
 */

import { createHash } from "node:crypto";

import { makeArtifactStore, type ArtifactStore } from "../db/artifacts.ts";
import {
  makeArtifactService,
  type ArtifactPullClient,
} from "../artifacts/service.ts";
import { sessions as defaultSessions } from "../control-plane/client.ts";
import type { SessionFileClient } from "../routes/session-files.ts";
import { productionSpecProjection } from "./projection.ts";
import { writeGuestFile } from "./projection.ts";
import type { SpecPublishArtifactPublisher, SpecTicketizeHandoff } from "./publish-scanner.ts";

/** Where the pinned render waits for the artifact pull. */
export function publishStagingPath(checkpointId: string): string {
  // The `.md` suffix decides the recorded media type, so it is not cosmetic.
  return `/workspace/.engrams/spec/published-${checkpointId}.md`;
}

export interface SpecPublishArtifactDeps {
  store: ArtifactStore;
  pull: ArtifactPullClient;
  guest: SessionFileClient;
}

export function makeSpecPublishArtifactPublisher(
  deps: SpecPublishArtifactDeps,
): SpecPublishArtifactPublisher {
  const encoder = new TextEncoder();

  async function recordedVersion(artifactId: string): Promise<{ version: number } | null> {
    const row = await deps.store.get(artifactId);
    return row ? { version: row.currentVersion } : null;
  }

  return {
    read: recordedVersion,

    async publish(input) {
      const path = publishStagingPath(input.checkpointId);
      const bytes = encoder.encode(input.markdown);
      const sha256 = createHash("sha256").update(bytes).digest("hex");
      await writeGuestFile(deps.guest, input.sessionId, path, bytes, sha256);
      // The id is fixed, so a second driver that reaches this line loses the
      // primary key and then reads the winner's version.
      const service = makeArtifactService({
        store: deps.store,
        pull: deps.pull,
        mintId: () => input.artifactId,
      });
      try {
        const artifact = await service.publish({
          sessionId: input.sessionId,
          taskId: null,
          ownerUserId: input.ownerUserId,
          filePath: path,
          title: input.title,
        });
        return { version: artifact.currentVersion };
      } catch (error) {
        const stored = await recordedVersion(input.artifactId);
        if (stored) return stored;
        throw error;
      }
    },
  };
}

/** The prompt that starts ticketization from the pinned spec (R36, mock 2l). */
export function ticketizePrompt(input: { specId: string; openQuestionCount: number }): string {
  const scope = JSON.stringify({ spec_id: input.specId });
  const lines = [
    "The spec is published. It is pinned, read-only, and it cannot be revised.",
    `Scope JSON: ${scope}`,
    "Read the pinned spec with spec_read before you propose anything.",
    "Propose the tickets that implement this spec. Give each ticket the section it comes from as its backlink.",
  ];
  if (input.openQuestionCount > 0) {
    lines.push(
      `The owner published with ${input.openQuestionCount} open questions. Carry each question on the ticket that covers its section, and never present it as answered.`,
    );
  }
  return lines.join("\n");
}

export interface SpecTicketizeClient {
  getSession(input: { sessionId: string }): Promise<{ session?: { status: string } }>;
  sendPrompt(input: { sessionId: string; promptId: string; text: string }): Promise<unknown>;
}

export interface SpecTicketizeHandoffDeps {
  sessions: SpecTicketizeClient;
  /** Refreshes the projection before the prompt lands, as every prompt does. */
  preparePrompt: (sessionId: string, status: string) => Promise<void>;
}

export function makeSpecTicketizeHandoff(deps: SpecTicketizeHandoffDeps): SpecTicketizeHandoff {
  return {
    async start(input) {
      const session = await deps.sessions.getSession({ sessionId: input.sessionId });
      await deps.preparePrompt(input.sessionId, session.session?.status ?? "");
      // The prompt id is derived from the spec, so a replayed step attaches to
      // the original prompt instead of asking for tickets twice.
      await deps.sessions.sendPrompt({
        sessionId: input.sessionId,
        promptId: input.promptId,
        text: ticketizePrompt(input),
      });
    },
  };
}

/** The production publisher, wired like the `Artifact` tool's own service. */
export function productionSpecPublishArtifactPublisher(): SpecPublishArtifactPublisher {
  return makeSpecPublishArtifactPublisher({
    store: makeArtifactStore(),
    pull: defaultSessions,
    guest: defaultSessions,
  });
}

export function productionSpecTicketizeHandoff(): SpecTicketizeHandoff {
  return makeSpecTicketizeHandoff({
    sessions: defaultSessions,
    preparePrompt: (sessionId, status) => productionSpecProjection.preparePrompt(sessionId, status),
  });
}
