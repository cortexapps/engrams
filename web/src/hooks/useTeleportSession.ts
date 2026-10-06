import { useQueryClient } from "@tanstack/react-query";
import { createConnectQueryKey, useMutation } from "@connectrpc/connect-query";
import { teleportSession } from "../gen/engram/app/v1/fleet-FleetService_connectquery";
import { getSession } from "../gen/engram/app/v1/session-SessionService_connectquery";

export function useTeleportSession(sessionId: string) {
  const qc = useQueryClient();
  const sessionKey = createConnectQueryKey({
    schema: getSession,
    input: { sessionId },
    cardinality: "finite",
  });
  const mutation = useMutation(teleportSession, {
    onSettled: () => {
      qc.invalidateQueries({ queryKey: sessionKey });
    },
  });
  return {
    mutate: (targetHostId: string) => mutation.mutate({ sessionId, targetHost: targetHostId }),
    mutateAsync: (targetHostId: string) =>
      mutation.mutateAsync({ sessionId, targetHost: targetHostId }),
    isPending: mutation.isPending,
    error: mutation.error,
    status: mutation.status,
    data: mutation.data,
  };
}
