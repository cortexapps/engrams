import { createConnectQueryKey, useMutation, useQuery } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";

import {
  listModelRouters,
  listRouterModels,
  refreshRouterModels,
  updateRouterModelPolicy,
} from "../gen/engram/app/v1/model_router-ModelRouterService_connectquery";
import { RouterModelAudience } from "../gen/engram/app/v1/model_router_pb";

export function useModelRouters() {
  return useQuery(listModelRouters, {}, { staleTime: 10_000 });
}

export function useRouterModels(
  routerId: string,
  search = "",
  audience = RouterModelAudience.ADMIN_CATALOG,
) {
  return useQuery(
    listRouterModels,
    { routerId, audience, search },
    { enabled: Boolean(routerId), staleTime: 10_000 },
  );
}

export function useInvalidateRouter(routerId: string, search: string) {
  const queryClient = useQueryClient();
  return async () => {
    await Promise.all([
      queryClient.invalidateQueries({
        queryKey: createConnectQueryKey({
          schema: listModelRouters,
          input: {},
          cardinality: "finite",
        }),
      }),
      queryClient.invalidateQueries({
        queryKey: createConnectQueryKey({
          schema: listRouterModels,
          input: { routerId, audience: RouterModelAudience.ADMIN_CATALOG, search },
          cardinality: "finite",
        }),
      }),
    ]);
  };
}

export function useRefreshRouterModels(routerId: string, search: string) {
  return useMutation(refreshRouterModels, { onSuccess: useInvalidateRouter(routerId, search) });
}

export function useUpdateRouterModelPolicy(routerId: string, search: string) {
  return useMutation(updateRouterModelPolicy, { onSuccess: useInvalidateRouter(routerId, search) });
}
