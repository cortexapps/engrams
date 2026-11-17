/** What a chat provider decides about its chat-threads built-in. It imports
 * no product module: a provider module reads it while the chat registry
 * loads. */

import type { BlockDef } from "../engine/definition.ts";

/** Where the facts step's value is, for a variant's templates: the place
 * of the mention, plus the profile the graph chose for it. */
export const THREADS_FACTS = "steps.facts.value";

export interface ThreadsVariant {
  /** The chat provider's connector provider. */
  provider: string;
  /** The built-in's key. It is stored; it does not change. */
  key: string;
  name: string;
  previousNames?: string[];
  description: string;
  /** Bump on any change to the definition this variant builds. */
  definitionVersion: number;
  /** The events that are a mention of the app. */
  mentionEvents: string[];
  /** The `place` step says where a mention is: `value` is null when the
   * event is no mention to answer, else `team`, `channel`, `thread_ts`,
   * `mention_ts`, `user_id`, `event_id`, `text` and `title`, plus what the
   * variant's own steps read. The graph's `facts` step adds the profile. */
  place: Omit<BlockDef, "id">;
  /** The `replies` step reads the thread, from `steps.previous.mention_ts`
   * on when a session continues. The `turn` step folds it into `value`:
   * `text` is the prompt, and `has_text` false ends the run. */
  replies: Omit<BlockDef, "id">;
  turn: Omit<BlockDef, "id">;
  /** Connector action ids. `post` takes `channel`, `threadTs` and `text`;
   * `update` takes `channel`, `ts` and `text` and removes a card's buttons. */
  actions: { post: string; update: string };
  /** What a channel key looks like, for the prompt of the channel map. */
  channelKeyHint: string;
  /** The workstream key and label of a thread, as templates over
   * `event.raw`. */
  threadKeyTemplate: string;
  threadLabelTemplate: string;
  /** Appended to the system prompt of a session the thread starts: how to
   * write for this chat tool. */
  systemPromptAppend: string;
}
