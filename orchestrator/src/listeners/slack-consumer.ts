import { DBOS } from "@dbos-inc/dbos-sdk";

import {
  THREAD_TOPIC,
  type ThreadInbox,
} from "../workflows/thread-inbox.ts";
import type { SessionConsumer } from "./consumer.ts";
import { makeSlackSessionStore } from "./slack-session-store.ts";

export type SlackMailboxSend = (
  destinationId: string,
  message: ThreadInbox,
  topic: string,
  idempotencyKey: string,
) => Promise<void>;

export interface SlackConsumerDeps {
  findThreadWorkflow(sessionId: string): Promise<string | null>;
  send: SlackMailboxSend;
}

export function makeSlackConsumer(deps: SlackConsumerDeps): SessionConsumer {
  let threadWfId: string | null | undefined;

  const destination = (): string => {
    if (!threadWfId) throw new Error("Slack consumer has no thread binding");
    return threadWfId;
  };

  return {
    name: "slack",
    interestedIn: () => true,
    async appliesTo(sessionId) {
      if (threadWfId === undefined) {
        threadWfId = await deps.findThreadWorkflow(sessionId);
      }
      return threadWfId !== null;
    },
    async handle(event, ctx) {
      await deps.send(
        destination(),
        { kind: "session_event", event },
        THREAD_TOPIC,
        `slack:${ctx.sessionId}:${event.idx}`,
      );
    },
    async onTerminal(outcome, ctx) {
      await deps.send(
        destination(),
        { kind: "session_terminal", outcome },
        THREAD_TOPIC,
        `slack:${ctx.sessionId}:terminal`,
      );
    },
  };
}

export function makeProductionSlackConsumer(): SessionConsumer {
  const store = makeSlackSessionStore();
  return makeSlackConsumer({
    findThreadWorkflow: (sessionId) => store.findThreadWorkflow(sessionId),
    send: async (destinationId, message, topic, idempotencyKey) => {
      await DBOS.send<ThreadInbox>(
        destinationId,
        message,
        topic,
        idempotencyKey,
      );
    },
  });
}
