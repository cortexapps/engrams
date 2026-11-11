/** The registered chat providers, by connector provider. */

import { SLACK_CHAT } from "../providers/slack/chat.ts";
import type { ThreadsVariant } from "../automations/builtins/threads-variant.ts";
import type { ChatProvider, ChatProviderSpec } from "./chat.ts";

const SPECS: ReadonlyMap<string, ChatProviderSpec> = new Map(
  [SLACK_CHAT].map((spec) => [spec.provider, spec]),
);
const instances = new Map<string, ChatProvider>();

export function chatSpecs(): ChatProviderSpec[] {
  return [...SPECS.values()];
}

/** The chat provider for `provider`, or null when the provider is not one. */
export function chatFor(provider: string): ChatProvider | null {
  const spec = SPECS.get(provider);
  if (!spec) return null;
  let chat = instances.get(provider);
  if (!chat) instances.set(provider, (chat = spec.make()));
  return chat;
}

export function requireChat(provider: string): ChatProvider {
  const chat = chatFor(provider);
  if (!chat) throw new Error(`"${provider}" is not a chat provider`);
  return chat;
}

/** The chat-threads built-in variants, one for each provider that has one. */
export function threadsVariants(): ThreadsVariant[] {
  return chatSpecs().flatMap((spec) => (spec.threads ? [spec.threads] : []));
}

/** The key of the provider's chat-threads built-in, or null. */
export function threadsBuiltinKey(provider: string): string | null {
  return SPECS.get(provider)?.threads?.key ?? null;
}

/** The provider that a stored row or block config with no provider means. */
export function defaultChatProvider(): string {
  for (const spec of SPECS.values()) if (spec.legacyDefault) return spec.provider;
  throw new Error("no chat provider is the legacy default");
}
