import { createContext, useCallback, useContext, type ReactNode } from "react";

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
  const submit = useCallback<SubmitBlockIteration>(
    async (request) => submitBlockIteration(specId, request),
    [specId],
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

export async function submitBlockIteration(
  specId: string,
  request: SpecBlockIterationRequest,
): Promise<void> {
  const response = await fetch(
    `/api/v1/specs/${encodeURIComponent(specId)}/blocks/${encodeURIComponent(request.blockId)}/messages`,
    {
      method: "POST",
      credentials: "same-origin",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ section_id: request.sectionId, message: request.message }),
    },
  );
  if (response.ok) return;
  let detail = `Request failed (${response.status})`;
  try {
    const body: unknown = await response.json();
    if (
      body !== null &&
      typeof body === "object" &&
      !Array.isArray(body) &&
      typeof Reflect.get(body, "error") === "string"
    ) {
      detail = Reflect.get(body, "error");
    }
  } catch {
    // Keep the status-derived message when the server did not return JSON.
  }
  throw new Error(detail);
}
