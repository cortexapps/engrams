import { useQuery } from "@tanstack/react-query";
import { fetchHosts } from "../api";

export function useHosts() {
  return useQuery({
    queryKey: ["hosts"],
    queryFn: fetchHosts,
    refetchInterval: 1000,
    refetchOnWindowFocus: false,
    staleTime: 0,
    placeholderData: (prev) => prev,
  });
}
