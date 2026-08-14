import { useMutation, useQueryClient } from "@tanstack/react-query";
import { createConnectQueryKey } from "@connectrpc/connect-query";

import { listSpecs } from "@/gen/engram/app/v1/spec-SpecService_connectquery";
import { specRequest } from "@/lib/spec-api";

export interface CreateSpecInput {
  templateId: string;
  profileId: string;
  problemStatement: string;
  harness?: string;
  model?: string;
  modelRouter?: string;
  effort?: string;
  harnessMode?: string;
  title?: string;
  /** The client's stable key for this create. The server derives the spec id
   *  from it, so a repeated send does not create a second spec. */
  idempotencyKey: string;
}

export interface CreatedSpec {
  id: string;
  title: string;
  sessionId: string;
  templateId: string;
  phase: "ideation";
}

export async function createSpec(input: CreateSpecInput): Promise<CreatedSpec> {
  return (
    await specRequest<{ spec: CreatedSpec }>("/specs", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(input),
    })
  ).spec;
}

export function useCreateSpec() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: createSpec,
    onSuccess: async () => {
      await queryClient.invalidateQueries({
        queryKey: createConnectQueryKey({ schema: listSpecs, cardinality: undefined }),
      });
    },
  });
}
