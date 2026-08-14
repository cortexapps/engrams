import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { specRequest } from "@/lib/spec-api";

export interface SpecMessage {
  promptId: string;
  author: { id: string | null; name: string };
  text: string;
  createdAt: string;
}

export interface SpecMessagesResult {
  messages: SpecMessage[];
  byPromptId: ReadonlyMap<string, SpecMessage>;
  /** An opaque server token to pass unchanged as the next `after` value. */
  nextAfter: string;
}

interface SpecMessagesResponse {
  messages: Array<{
    prompt_id: string;
    author: { id: string | null; name: string };
    text: string;
    created_at: string;
  }>;
  next_after: string;
}

/** Read clean, attributed human turns after the supplied server cursor. */
export async function getSpecMessages(specId: string, after = ""): Promise<SpecMessagesResult> {
  const response = await specRequest<SpecMessagesResponse>(
    `/specs/${encodeURIComponent(specId)}/messages?after=${encodeURIComponent(after)}`,
  );
  const messages = response.messages.map((message) => ({
    promptId: message.prompt_id,
    author: message.author,
    text: message.text,
    createdAt: message.created_at,
  }));
  return mergeSpecMessagePages([{ messages, nextAfter: response.next_after }]);
}

/** Merge loaded pages without rendering a re-delivered boundary prompt twice. */
export function mergeSpecMessagePages(
  pages: ReadonlyArray<Pick<SpecMessagesResult, "messages" | "nextAfter">>,
): SpecMessagesResult {
  const byPromptId = new Map<string, SpecMessage>();
  for (const page of pages) {
    for (const message of page.messages) byPromptId.set(message.promptId, message);
  }
  return {
    messages: [...byPromptId.values()],
    byPromptId,
    nextAfter: pages.at(-1)?.nextAfter ?? "",
  };
}

/** Send one clean human turn to the shared spec session. */
export function postSpecMessage(specId: string, message: string): Promise<{ promptId: string }> {
  return specRequest<{ prompt_id: string }>(`/specs/${encodeURIComponent(specId)}/messages`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ message }),
  }).then((response) => ({ promptId: response.prompt_id }));
}

export function useSpecMessages(specId: string, after = "") {
  return useQuery({
    queryKey: ["spec", specId, "messages", after],
    queryFn: () => getSpecMessages(specId, after),
    enabled: specId.length > 0,
  });
}

export function useSendSpecMessage(specId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (message: string) => postSpecMessage(specId, message),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["spec", specId, "messages"] }),
  });
}
