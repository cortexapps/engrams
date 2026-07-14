import type {
  CuratedEvent,
  TerminalOutcome,
} from "../control-plane/session-events.ts";

export interface SessionConsumerContext {
  sessionId: string;
}

export interface SessionConsumer {
  name: string;
  interestedIn(kind: string): boolean;
  appliesTo(sessionId: string): Promise<boolean>;
  handle(event: CuratedEvent, ctx: SessionConsumerContext): Promise<void>;
  onTerminal?(
    outcome: TerminalOutcome,
    ctx: SessionConsumerContext,
  ): Promise<void>;
}
