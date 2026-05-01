import { useQuery } from '@tanstack/react-query';
import { fetchImages } from '../api';

// Image lists are cheap (filesystem walk on the coordinator) but they
// don't change between bake runs. Refetch on window focus and when the
// "new session" form opens — that's enough to pick up a freshly baked
// image without polling.
export function useImages(enabled: boolean) {
  return useQuery({
    queryKey: ['images'],
    queryFn: fetchImages,
    enabled,
    refetchOnWindowFocus: true,
    staleTime: 30_000,
  });
}
