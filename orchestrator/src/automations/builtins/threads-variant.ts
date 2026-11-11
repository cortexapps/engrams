/** What a chat provider decides about its chat-threads built-in. It imports
 * no product module: a provider module reads it while the chat registry
 * loads. */

import type { BlockDef } from "../engine/definition.ts";

/** Where the facts step's value is, for a variant's templates. */
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
  /** The `facts` step: `value` is null to reject the event, else `admit`,
   * `team`, `channel`, `thread_ts`, `mention_ts`, `user_id`, `event_id`,
   * `title` and `profile_id`, plus what the variant's own steps read. */
  facts: BlockDef;
  /** The `replies` step reads the thread, from `steps.previous.mention_ts`
   * on when a session continues. The `turn` step folds it into
   * `value.text`, the prompt; an empty text ends the run. */
  replies: BlockDef;
  turn: BlockDef;
  /** Connector action ids. `post` takes `channel`, `threadTs` and `text`;
   * `update` takes `channel`, `ts` and `text` and removes a card's buttons. */
  actions: { post: string; update: string };
  /** The workstream key and label of a thread, as templates over
   * `event.raw`. */
  threadKeyTemplate: string;
  threadLabelTemplate: string;
  /** Appended to the system prompt of a session the thread starts: how to
   * write for this chat tool. */
  systemPromptAppend: string;
}
