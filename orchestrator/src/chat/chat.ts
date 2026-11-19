/** The chat role: what the product needs from a chat tool to run an agent in
 * a thread. A provider implements `ChatProvider` under `providers/<name>/`
 * and joins `registry.ts`. */

import type { Hono } from "hono";

import type { ThreadsVariant } from "../automations/builtins/threads-variant.ts";
import type { CommunicationPolicy, SourceMention } from "../workflows/communication-policy.ts";

/** Where the answer to an interactive card goes. The provider carries it
 * on the card, and `answers.ts` delivers the answer. */
export interface AnswerRoute {
  /** Present when a run posted the card (a choice block). */
  runId?: string;
  /** Present when a session relay posted the card (an agent's question). */
  sessionId?: string;
}

export interface ChannelOption {
  key: string;
  label: string;
}

export interface ChatProvider {
  readonly provider: string;
  /** The renderer of a session into a thread. */
  policy(route: AnswerRoute): CommunicationPolicy;
  /** The email address of a chat user; null when the provider gives none. */
  userEmail(externalUserId: string): Promise<string | null>;
  /** Every channel the app can see. */
  listChannels(): Promise<ChannelOption[]>;
  /** A label for each channel id, in request order. */
  describeChannels(keys: readonly string[]): Promise<ChannelOption[]>;
  /** A message's text as a person reads it: no user mentions, and a
   * channel or a link as its visible part. */
  plainText(text: string): string;
  /** The workstream handle of a thread. */
  threadHandle(place: Pick<SourceMention, "channel" | "threadRoot">): string;
}

export interface ChatProviderSpec {
  /** Stored rows and block configs with no provider mean this one. */
  legacyDefault?: true;
  make(): ChatProvider;
  /** The app that takes card clicks. It is mounted at the provider's
   * interactivity path when the connector's ingress declares one, and it
   * handles `POST /`. The path takes requests with no login, so the app
   * verifies the sender. */
  interactivity?: () => Hono;
  /** The provider's chat-threads built-in. */
  threads?: ThreadsVariant;
}
