/** The chat providers, keyed by the provider name of their connector. */

import type { ThreadsVariant } from "../automations/builtins/threads-variant.ts";
import { providerSpecs } from "../providers/index.ts";
import type { ChatProvider, ChatProviderSpec } from "./chat.ts";

const instances = new WeakMap<ChatProviderSpec, ChatProvider>();

/** Each chat provider's name and spec. */
export function chatSpecs(): Array<{ provider: string; spec: ChatProviderSpec }> {
  return providerSpecs().flatMap(({ provider, chat }) => (chat ? [{ provider, spec: chat }] : []));
}

function specFor(provider: string): ChatProviderSpec | null {
  return chatSpecs().find((entry) => entry.provider === provider)?.spec ?? null;
}

/** The chat provider for `provider`, or null when the provider is not one. */
export function chatFor(provider: string): ChatProvider | null {
  const spec = specFor(provider);
  if (!spec) return null;
  let chat = instances.get(spec);
  if (!chat) instances.set(spec, (chat = spec.make()));
  return chat;
}

export function requireChat(provider: string): ChatProvider {
  const chat = chatFor(provider);
  if (!chat) throw new Error(`"${provider}" is not a chat provider`);
  return chat;
}

/** The chat-threads built-in variants, one for each provider that has one. */
export function threadsVariants(): ThreadsVariant[] {
  return chatSpecs().flatMap(({ spec }) => (spec.threads ? [spec.threads] : []));
}

/** The key of the provider's chat-threads built-in, or null. */
export function threadsBuiltinKey(provider: string): string | null {
  return specFor(provider)?.threads?.key ?? null;
}

/** The provider that a stored row or block config with no provider means. */
export function defaultChatProvider(): string {
  for (const { provider, spec } of chatSpecs()) if (spec.legacyDefault) return provider;
  throw new Error("no chat provider is the legacy default");
}
