import { createContext, useCallback, useContext, type ReactNode } from "react";

import { useSendSpecMessage } from "@/hooks/useSpecMessages";

export interface SpecBlockIterationRequest {
  sectionId: string;
  blockId: string;
  message: string;
}

type SubmitBlockIteration = (request: SpecBlockIterationRequest) => Promise<void>;

const SpecBlockIterationContext = createContext<SubmitBlockIteration | null>(null);

export function SpecBlockIterationProvider({
  specId,
  children,
}: {
  specId: string;
  children: ReactNode;
}) {
  const sendMessage = useSendSpecMessage(specId);
  const submit = useCallback<SubmitBlockIteration>(
    async (request) => {
      await sendMessage.mutateAsync(blockIterationMessage(request));
    },
    [sendMessage],
  );
  return (
    <SpecBlockIterationContext.Provider value={submit}>
      {children}
    </SpecBlockIterationContext.Provider>
  );
}

export function useSpecBlockIteration(): SubmitBlockIteration | null {
  return useContext(SpecBlockIterationContext);
}

export function blockIterationMessage(request: SpecBlockIterationRequest): string {
  return `Use spec_update_block to update block ${request.blockId} in §${request.sectionId}: ${request.message}`;
}
