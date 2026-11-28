/** Replica-local abort signals. Authority remains in durable state, not this registry. */
export function makeMcpActiveWatch(deps: { check(id: string): Promise<void>; intervalMs: number }) {
  const active = new Map<string, Set<AbortController>>();
  return {
    register(id: string) {
      const controller = new AbortController();
      const controllers = active.get(id) ?? new Set<AbortController>();
      controllers.add(controller);
      active.set(id, controllers);
      let closed = false;
      let failure: unknown;
      let timer: ReturnType<typeof setTimeout> | undefined;
      const check = async (): Promise<void> => {
        if (closed || controller.signal.aborted) return;
        let timeout: ReturnType<typeof setTimeout> | undefined;
        try {
          await Promise.race([
            deps.check(id),
            new Promise<never>((_, reject) => {
              timeout = setTimeout(
                () => reject(Error("MCP authority check timed out")),
                deps.intervalMs,
              );
            }),
          ]);
        } catch (error) {
          failure = error;
          controller.abort();
        } finally {
          if (timeout) clearTimeout(timeout);
        }
        if (!closed && !controller.signal.aborted) timer = setTimeout(check, deps.intervalMs);
      };
      // Defer the first read until registration is visible to local cancellation.
      const ready = Promise.resolve().then(check);
      return {
        signal: controller.signal,
        ready,
        throwIfFailed() {
          if (failure !== undefined) throw failure;
        },
        close() {
          closed = true;
          if (timer) clearTimeout(timer);
          controllers.delete(controller);
          if (!controllers.size && active.get(id) === controllers) active.delete(id);
        },
      };
    },
    cancel(id: string) {
      for (const controller of active.get(id) ?? []) controller.abort();
    },
  };
}
