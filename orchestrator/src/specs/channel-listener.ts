export interface SpecChannelNotification {
  channel: string;
  payload?: string;
}

export interface SpecChannelClient {
  on(event: "notification", listener: (message: SpecChannelNotification) => void): void;
  on(event: "error", listener: (error: Error) => void): void;
  off(event: "notification", listener: (message: SpecChannelNotification) => void): void;
  off(event: "error", listener: (error: Error) => void): void;
  query(queryText: string): Promise<unknown>;
  release(destroy?: boolean): void;
}

export interface SpecChannelPool {
  connect(): Promise<SpecChannelClient>;
}

export interface SpecChannelListenerOptions {
  onWarning?: (message: string) => void;
  onReconnect?: () => void;
  delay?: (milliseconds: number, signal: AbortSignal) => Promise<void>;
  initialBackoffMs?: number;
  maximumBackoffMs?: number;
  label?: string;
}

interface ActiveListener {
  client: SpecChannelClient;
  onNotification: (message: SpecChannelNotification) => void;
  onError: (error: Error) => void;
}

const INITIAL_BACKOFF_MS = 250;
const MAXIMUM_BACKOFF_MS = 10_000;

/** Keep one PostgreSQL LISTEN connection active until the caller stops it. */
export async function listenForSpecChannel(
  pool: SpecChannelPool,
  channel: string,
  onNotification: (message: SpecChannelNotification) => void,
  options: SpecChannelListenerOptions = {},
): Promise<() => Promise<void>> {
  const abort = new AbortController();
  const delay = options.delay ?? abortableDelay;
  const initialBackoffMs = options.initialBackoffMs ?? INITIAL_BACKOFF_MS;
  const maximumBackoffMs = options.maximumBackoffMs ?? MAXIMUM_BACKOFF_MS;
  const label = options.label ?? "Spec PostgreSQL listener";
  let stopped = false;
  let active: ActiveListener | null = null;
  let reconnecting: Promise<void> | null = null;

  const detach = (listener: ActiveListener, destroy: boolean): void => {
    listener.client.off("notification", listener.onNotification);
    listener.client.off("error", listener.onError);
    listener.client.release(destroy);
  };

  const reconnect = async (): Promise<void> => {
    let backoffMs = initialBackoffMs;
    while (!stopped) {
      try {
        await delay(backoffMs, abort.signal);
      } catch (error: unknown) {
        if (stopped) return;
        options.onWarning?.(`${label} reconnect delay failed: ${errorMessage(error)}`);
        backoffMs = Math.min(backoffMs * 2, maximumBackoffMs);
        continue;
      }
      if (stopped) return;
      try {
        await open();
        if (stopped) return;
        try {
          options.onReconnect?.();
        } catch (error: unknown) {
          options.onWarning?.(`${label} recovery failed: ${errorMessage(error)}`);
        }
        return;
      } catch (error: unknown) {
        if (stopped) return;
        options.onWarning?.(`${label} reconnect failed: ${errorMessage(error)}`);
        backoffMs = Math.min(backoffMs * 2, maximumBackoffMs);
      }
    }
  };

  const open = async (): Promise<void> => {
    const client = await pool.connect();
    let connectionError: Error | null = null;
    let ready = false;
    const listener: ActiveListener = {
      client,
      onNotification,
      onError: (error) => {
        if (!ready) {
          connectionError = error;
          return;
        }
        if (stopped || active !== listener) return;
        active = null;
        detach(listener, true);
        options.onWarning?.(`${label} failed: ${error.message}; reconnecting`);
        reconnecting ??= reconnect().finally(() => {
          reconnecting = null;
        });
      },
    };
    client.on("notification", listener.onNotification);
    client.on("error", listener.onError);
    try {
      await client.query(`LISTEN ${channel}`);
      if (connectionError) throw connectionError;
      if (stopped) {
        client.off("notification", listener.onNotification);
        await client.query(`UNLISTEN ${channel}`).catch(() => {});
        client.off("error", listener.onError);
        client.release();
        return;
      }
      active = listener;
      ready = true;
    } catch (error) {
      client.off("notification", listener.onNotification);
      client.off("error", listener.onError);
      client.release(true);
      throw error;
    }
  };

  await open();

  return async () => {
    if (stopped) return;
    stopped = true;
    abort.abort();
    const listener = active;
    active = null;
    if (listener) {
      listener.client.off("notification", listener.onNotification);
      await listener.client.query(`UNLISTEN ${channel}`).catch(() => {});
      listener.client.off("error", listener.onError);
      listener.client.release();
    }
    await reconnecting;
  };
}

function abortableDelay(milliseconds: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    if (signal.aborted) {
      resolve();
      return;
    }
    const timer = setTimeout(finish, milliseconds);
    signal.addEventListener("abort", finish, { once: true });

    function finish(): void {
      clearTimeout(timer);
      signal.removeEventListener("abort", finish);
      resolve();
    }
  });
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
